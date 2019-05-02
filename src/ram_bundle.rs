use scroll::Pread;
use std::ops::Range;

use crate::builder::SourceMapBuilder;
use crate::errors::{Error, Result};
use crate::sourceview::SourceView;
use crate::types::{SourceMap, SourceMapIndex};

const RAM_BUNDLE_MAGIC: u32 = 0xFB0B_D1E5;

#[derive(Debug, Pread, Clone, Copy)]
#[repr(C, packed)]
pub struct RamBundleHeader {
    magic: u32,
    module_count: u32,
    startup_code_size: u32,
}

impl RamBundleHeader {
    /// Checks if the magic matches.
    pub fn is_valid_magic(&self) -> bool {
        self.magic == RAM_BUNDLE_MAGIC
    }
}

#[derive(Debug, Pread, Clone, Copy)]
#[repr(C, packed)]
struct ModuleEntry {
    offset: u32,
    length: u32,
}

#[derive(Debug)]
pub struct RamBundleModule<'a> {
    id: usize,
    data: &'a [u8],
}

impl<'a> RamBundleModule<'a> {
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn source_view(&self) -> Result<SourceView<'a>> {
        match std::str::from_utf8(self.data) {
            Ok(s) => Ok(SourceView::new(s)),
            Err(e) => Err(Error::Utf8(e)),
        }
    }
}

pub struct RamBundleModuleIter<'a, 'b> {
    range: Range<usize>,
    ram_bundle: &'b RamBundle<'a>,
}

impl<'a> Iterator for RamBundleModuleIter<'a, '_> {
    type Item = Result<RamBundleModule<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(next_id) = self.range.next() {
            match self.ram_bundle.get_module(next_id) {
                Ok(None) => continue,
                Ok(Some(module)) => return Some(Ok(module)),
                Err(e) => return Some(Err(e)),
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RamBundle<'a> {
    bytes: &'a [u8],
    module_count: usize,
    startup_code_size: usize,
    startup_code_offset: usize,
}

impl ModuleEntry {
    pub fn is_empty(&self) -> bool {
        self.offset == 0 && self.length == 0
    }
}

impl<'a> RamBundle<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self> {
        let header = bytes.pread_with::<RamBundleHeader>(0, scroll::LE)?;

        if !header.is_valid_magic() {
            return Err(Error::InvalidRamBundleMagic);
        }

        let module_count = header.module_count as usize;
        let startup_code_offset = std::mem::size_of::<RamBundleHeader>()
            + module_count * std::mem::size_of::<ModuleEntry>();
        Ok(RamBundle {
            bytes,
            module_count,
            startup_code_size: header.startup_code_size as usize,
            startup_code_offset,
        })
    }

    pub fn module_count(&self) -> usize {
        self.module_count
    }

    pub fn startup_code_size(&self) -> usize {
        self.startup_code_size
    }

    pub fn startup_code_offset(&self) -> usize {
        self.startup_code_offset
    }

    pub fn startup_code(&self) -> Result<&'a [u8]> {
        self.bytes
            .pread_with(self.startup_code_offset, self.startup_code_size)
            .map_err(Error::Scroll)
    }

    pub fn get_module(&self, id: usize) -> Result<Option<RamBundleModule<'a>>> {
        if id >= self.module_count {
            return Err(Error::InvalidRamBundleIndex);
        }

        let entry_offset =
            std::mem::size_of::<RamBundleHeader>() + id * std::mem::size_of::<ModuleEntry>();

        let module_entry = self
            .bytes
            .pread_with::<ModuleEntry>(entry_offset, scroll::LE)?;

        if module_entry.is_empty() {
            return Ok(None);
        }

        let module_global_offset = self.startup_code_offset + module_entry.offset as usize;

        if module_entry.length == 0 {
            return Err(Error::InvalidRamBundleEntry);
        }

        // Strip the trailing NULL byte
        let module_length = (module_entry.length - 1) as usize;
        let data = self.bytes.pread_with(module_global_offset, module_length)?;

        Ok(Some(RamBundleModule { id, data }))
    }

    pub fn iter_modules(&self) -> RamBundleModuleIter<'a, '_> {
        RamBundleModuleIter {
            range: 0..self.module_count,
            ram_bundle: self,
        }
    }
}

pub struct SplitRamBundleModuleIter<'a, 'b> {
    ram_bundle_iter: RamBundleModuleIter<'a, 'b>,
    sm: SourceMap,
    offsets: Vec<Option<u32>>,
}

impl<'a> SplitRamBundleModuleIter<'a, '_> {
    fn split_module(
        &self,
        module: RamBundleModule<'a>,
    ) -> Result<Option<(String, SourceView<'a>, SourceMap)>> {
        let module_offset = self
            .offsets
            .get(module.id())
            .ok_or(Error::InvalidRamBundleIndex)?;
        let starting_line = match *module_offset {
            Some(offset) => offset,
            None => return Ok(None),
        };

        let mut token_iter = self.sm.tokens();

        if !token_iter.seek(starting_line, 0) {
            return Err(Error::InvalidRamBundleEntry);
        }

        let source: SourceView<'a> = module.source_view()?;
        let line_count = source.line_count() as u32;
        let ending_line = starting_line + line_count;
        let last_line_len = source
            .get_line(line_count - 1)
            .map_or(0, |line| line.chars().map(|c| c.len_utf16()).sum())
            as u32;

        let filename = format!("{}.js", module.id);
        let mut builder = SourceMapBuilder::new(Some(&filename));
        for token in token_iter {
            let dst_line = token.get_dst_line();
            let dst_col = token.get_dst_col();

            if dst_line >= ending_line || dst_col >= last_line_len {
                break;
            }

            let raw = builder.add(
                dst_line - starting_line,
                dst_col,
                token.get_src_line(),
                token.get_src_col(),
                token.get_source(),
                token.get_name(),
            );
            if token.get_source().is_some() && !builder.has_source_contents(raw.src_id) {
                builder.set_source_contents(
                    raw.src_id,
                    self.sm.get_source_contents(token.get_src_id()),
                );
            }
        }
        let sourcemap = builder.into_sourcemap();
        return Ok(Some((filename, source, sourcemap)));
    }
}

impl<'a> Iterator for SplitRamBundleModuleIter<'a, '_> {
    type Item = Result<(String, SourceView<'a>, SourceMap)>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(module_result) = self.ram_bundle_iter.next() {
            match module_result {
                Ok(module) => match self.split_module(module) {
                    Ok(None) => continue,
                    Ok(Some(result_tuple)) => return Some(Ok(result_tuple)),
                    Err(_) => return Some(Err(Error::InvalidRamBundleEntry)),
                },
                Err(_) => return Some(Err(Error::InvalidRamBundleEntry)),
            }
        }
        None
    }
}

pub fn split_ram_bundle<'a, 'b>(
    ram_bundle: &'b RamBundle<'a>,
    smi: &SourceMapIndex,
) -> Result<SplitRamBundleModuleIter<'a, 'b>> {
    Ok(SplitRamBundleModuleIter {
        ram_bundle_iter: ram_bundle.iter_modules(),
        sm: smi.flatten()?,
        offsets: smi
            .x_facebook_offsets()
            .map(|v| v.to_vec())
            .ok_or(Error::NotARamBundle)?,
    })
}
