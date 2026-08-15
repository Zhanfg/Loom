from pathlib import Path

script = Path("scripts/stage8_reader_refactor.py")
source = script.read_text()
old = '''    "crates/loom-ext4/src/remove.rs": (
        "    fn compile_remove_file(\\n",
        "    pub(crate) fn compile_remove_file(\\n",
    ),'''
new = '''    "crates/loom-ext4/src/remove.rs": (
        "    fn compile_remove_file(&mut self, target_path: &str) -> Result<CompiledRemoveFile, Ext4Error> {\\n",
        "    pub(crate) fn compile_remove_file(&mut self, target_path: &str) -> Result<CompiledRemoveFile, Ext4Error> {\\n",
    ),'''
if old not in source:
    raise SystemExit("Stage 8 remove matcher source not found")
source = source.replace(old, new, 1)
exec(compile(source, str(script), "exec"))
