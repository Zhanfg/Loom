from pathlib import Path

core = Path('crates/loom-erofs/src/compact_core.rs')
text = core.read_text()
old = '''    fn open(path: &Path) -> Result<Self, CoreError> {\n        let mut file = File::open(path).map_err(CoreError::Io)?;\n        let bytes = file.metadata().map_err(CoreError::Io)?.len();\n        let sb = read_superblock(&mut file, bytes)?;\n        Ok(Self { file, bytes, sb })\n    }\n'''
new = '''    fn open(path: &Path) -> Result<Self, CoreError> {\n        let mut file = File::open(path).map_err(CoreError::Io)?;\n        let metadata_bytes = file.metadata().map_err(CoreError::Io)?.len();\n        let bytes = if metadata_bytes != 0 {\n            metadata_bytes\n        } else {\n            let current = file.stream_position().map_err(CoreError::Io)?;\n            let end = file.seek(SeekFrom::End(0)).map_err(CoreError::Io)?;\n            file.seek(SeekFrom::Start(current)).map_err(CoreError::Io)?;\n            end\n        };\n        if bytes == 0 {\n            return Err(CoreError::InvalidFilesystem(\n                \"origin image or block device reports zero bytes\",\n            ));\n        }\n        let sb = read_superblock(&mut file, bytes)?;\n        Ok(Self { file, bytes, sb })\n    }\n'''
if old not in text:
    raise SystemExit('Image::open anchor not found')
core.write_text(text.replace(old, new, 1))

view = Path('crates/loom-view/src/lib.rs')
text = view.read_text()
old = '''        let origin = File::open(origin_path).map_err(ViewError::Io)?;\n        let image_bytes = origin.metadata().map_err(ViewError::Io)?.len();\n        if image_bytes == 0 || image_bytes % u64::from(block_size) != 0 {\n'''
new = '''        let mut origin = File::open(origin_path).map_err(ViewError::Io)?;\n        let metadata_bytes = origin.metadata().map_err(ViewError::Io)?.len();\n        let image_bytes = if metadata_bytes != 0 {\n            metadata_bytes\n        } else {\n            let current = origin.stream_position().map_err(ViewError::Io)?;\n            let end = origin.seek(SeekFrom::End(0)).map_err(ViewError::Io)?;\n            origin\n                .seek(SeekFrom::Start(current))\n                .map_err(ViewError::Io)?;\n            end\n        };\n        if image_bytes == 0 || image_bytes % u64::from(block_size) != 0 {\n'''
if old not in text:
    raise SystemExit('EffectiveBlockStore::open anchor not found')
view.write_text(text.replace(old, new, 1))
