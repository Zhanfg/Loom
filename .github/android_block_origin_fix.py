from pathlib import Path

path = Path('crates/loom-erofs/src/compact_core.rs')
text = path.read_text()
old = '''    fn open(path: &Path) -> Result<Self, CoreError> {\n        let mut file = File::open(path).map_err(CoreError::Io)?;\n        let bytes = file.metadata().map_err(CoreError::Io)?.len();\n        let sb = read_superblock(&mut file, bytes)?;\n        Ok(Self { file, bytes, sb })\n    }\n'''
new = '''    fn open(path: &Path) -> Result<Self, CoreError> {\n        let mut file = File::open(path).map_err(CoreError::Io)?;\n        let metadata_bytes = file.metadata().map_err(CoreError::Io)?.len();\n        let bytes = if metadata_bytes != 0 {\n            metadata_bytes\n        } else {\n            let current = file.stream_position().map_err(CoreError::Io)?;\n            let end = file.seek(SeekFrom::End(0)).map_err(CoreError::Io)?;\n            file.seek(SeekFrom::Start(current)).map_err(CoreError::Io)?;\n            end\n        };\n        if bytes == 0 {\n            return Err(CoreError::InvalidFilesystem(\n                \"origin image or block device reports zero bytes\",\n            ));\n        }\n        let sb = read_superblock(&mut file, bytes)?;\n        Ok(Self { file, bytes, sb })\n    }\n'''
if old not in text:
    raise SystemExit('Image::open anchor not found')
path.write_text(text.replace(old, new, 1))
