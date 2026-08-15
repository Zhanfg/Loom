from pathlib import Path

lib = Path("crates/loom-ext4/src/lib.rs")
s = lib.read_text()

marker = "#[derive(Debug)]\npub struct CompiledReplacement {"
insertion = """/// Seekable read source consumed by the ext4 compiler.
pub trait ImageReader: Read + Seek {}

impl<T: Read + Seek> ImageReader for T {}

/// One compiler session over an arbitrary immutable effective-image reader.
pub struct Ext4Session {
    image: Ext4Image,
}

impl Ext4Session {
    /// Opens an ext4 compiler session over a virtual image reader.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when the supplied image is malformed or unsupported.
    pub fn from_reader<R>(reader: R, image_bytes: u64) -> Result<Self, Ext4Error>
    where
        R: ImageReader + 'static,
    {
        Ok(Self {
            image: Ext4Image::from_reader(Box::new(reader), image_bytes)?,
        })
    }

    /// Compiles a same-size replacement against the session's current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when the path or replacement is invalid.
    pub fn replace(
        &mut self,
        target_path: &str,
        replacement: &[u8],
    ) -> Result<CompiledReplacement, Ext4Error> {
        let inode = self.image.resolve_path(target_path)?;
        self.image.compile_regular_replacement(inode, replacement)
    }

    /// Compiles a within-allocation resize against the current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when the resize violates ext4 Stage 2 invariants.
    pub fn resize(
        &mut self,
        target_path: &str,
        replacement: &[u8],
    ) -> Result<CompiledResize, Ext4Error> {
        let inode = self.image.resolve_path(target_path)?;
        self.image.compile_resize(inode, replacement)
    }

    /// Compiles one-block growth against the current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when allocator or extent invariants are not satisfied.
    pub fn grow(
        &mut self,
        target_path: &str,
        replacement: &[u8],
    ) -> Result<CompiledAllocationGrow, Ext4Error> {
        let inode = self.image.resolve_path(target_path)?;
        self.image.compile_one_block_growth(inode, replacement)
    }

    /// Compiles creation of one regular file against the current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when allocation or directory invariants are not satisfied.
    pub fn create(
        &mut self,
        target_path: &str,
        payload: &[u8],
    ) -> Result<CompiledCreateFile, Ext4Error> {
        self.image.compile_create_file(target_path, payload)
    }

    /// Compiles removal of one regular file from the current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when removal invariants are not satisfied.
    pub fn remove(&mut self, target_path: &str) -> Result<CompiledRemoveFile, Ext4Error> {
        self.image.compile_remove_file(target_path)
    }

    /// Compiles an in-inode `security.selinux` xattr against the current effective view.
    ///
    /// # Errors
    /// Returns [`Ext4Error`] when the xattr cannot be represented safely.
    pub fn selinux(
        &mut self,
        target_path: &str,
        value: &[u8],
    ) -> Result<CompiledSelinuxXattr, Ext4Error> {
        self.image.compile_selinux_xattr_bytes(target_path, value)
    }
}

#[derive(Debug)]
pub struct CompiledReplacement {"""
if insertion not in s:
    if marker not in s:
        raise SystemExit("CompiledReplacement marker not found")
    s = s.replace(marker, insertion, 1)

old_struct = "struct Ext4Image {\n    file: File,"
new_struct = "struct Ext4Image {\n    file: Box<dyn ImageReader>,"
if old_struct not in s:
    raise SystemExit("Ext4Image file field not found")
s = s.replace(old_struct, new_struct, 1)

old_open = """    fn open(path: &Path) -> Result<Self, Ext4Error> {
        let mut file = File::open(path).map_err(Ext4Error::Io)?;
        let image_bytes = file.metadata().map_err(Ext4Error::Io)?.len();
        if image_bytes % SECTOR_SIZE != 0 {
            return Err(Ext4Error::InvalidFilesystem(
                "origin size is not a multiple of 512 bytes",
            ));
        }

        let mut bytes = [0_u8; SUPERBLOCK_SIZE];
        read_exact_at(&mut file, SUPERBLOCK_OFFSET, &mut bytes)?;
        let superblock = Superblock::parse(&bytes)?;
        let fs_bytes = superblock
            .blocks_count
            .checked_mul(u64::from(superblock.block_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        if fs_bytes > image_bytes {
            return Err(Ext4Error::InvalidFilesystem(
                "filesystem block count exceeds origin size",
            ));
        }

        Ok(Self {
            file,
            image_bytes,
            superblock,
        })
    }
"""
new_open = """    fn open(path: &Path) -> Result<Self, Ext4Error> {
        let file = File::open(path).map_err(Ext4Error::Io)?;
        let image_bytes = file.metadata().map_err(Ext4Error::Io)?.len();
        Self::from_reader(Box::new(file), image_bytes)
    }

    fn from_reader(
        mut file: Box<dyn ImageReader>,
        image_bytes: u64,
    ) -> Result<Self, Ext4Error> {
        if image_bytes % SECTOR_SIZE != 0 {
            return Err(Ext4Error::InvalidFilesystem(
                "origin size is not a multiple of 512 bytes",
            ));
        }

        let mut bytes = [0_u8; SUPERBLOCK_SIZE];
        read_exact_at(&mut file, SUPERBLOCK_OFFSET, &mut bytes)?;
        let superblock = Superblock::parse(&bytes)?;
        let fs_bytes = superblock
            .blocks_count
            .checked_mul(u64::from(superblock.block_size))
            .ok_or(Ext4Error::ArithmeticOverflow)?;
        if fs_bytes > image_bytes {
            return Err(Ext4Error::InvalidFilesystem(
                "filesystem block count exceeds origin size",
            ));
        }

        Ok(Self {
            file,
            image_bytes,
            superblock,
        })
    }
"""
if old_open not in s:
    raise SystemExit("Ext4Image::open block not found")
s = s.replace(old_open, new_open, 1)

old_read = "fn read_exact_at(file: &mut File, offset: u64, buffer: &mut [u8]) -> Result<(), Ext4Error> {"
new_read = "fn read_exact_at<R: Read + Seek + ?Sized>(file: &mut R, offset: u64, buffer: &mut [u8]) -> Result<(), Ext4Error> {"
if old_read not in s:
    raise SystemExit("read_exact_at signature not found")
s = s.replace(old_read, new_read, 1)
lib.write_text(s)

method_changes = {
    "crates/loom-ext4/src/allocate.rs": (
        "    fn compile_one_block_growth(\n",
        "    pub(crate) fn compile_one_block_growth(\n",
    ),
    "crates/loom-ext4/src/resize.rs": (
        "    fn compile_resize(\n",
        "    pub(crate) fn compile_resize(\n",
    ),
    "crates/loom-ext4/src/create.rs": (
        "    fn compile_create_file(\n",
        "    pub(crate) fn compile_create_file(\n",
    ),
    "crates/loom-ext4/src/remove.rs": (
        "    fn compile_remove_file(\n",
        "    pub(crate) fn compile_remove_file(\n",
    ),
}
for file_name, (old, new) in method_changes.items():
    path = Path(file_name)
    text = path.read_text()
    if old not in text:
        raise SystemExit(f"method marker not found in {file_name}")
    path.write_text(text.replace(old, new, 1))
