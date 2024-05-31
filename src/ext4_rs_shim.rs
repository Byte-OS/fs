use core::iter::zip;

use alloc::{
    ffi::CString,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use devices::get_blk_device;

use sync::Mutex;
use vfscore::{
    DirEntry, FileSystem, FileType, INodeInterface, Metadata, OpenFlags, StatFS, StatMode,
    TimeSpec, VfsError, VfsResult,
};

use ext4_rs::*;

const BLOCK_SIZE: usize = 4096;

#[derive(Debug)]
pub struct Ext4Disk {
    device_id: usize,
}

impl Ext4Disk {
    /// Create a new disk.
    pub fn new(device_id: usize) -> Self {
        Self { device_id }
    }
}

impl BlockDevice for Ext4Disk {
    fn read_offset(&self, offset: usize) -> Vec<u8> {
        // log::info!("read_offset: {:x?}", offset);
        let mut buf = vec![0; BLOCK_SIZE];
        let device = get_blk_device(self.device_id).unwrap();

        let start_block_id = offset / 512;
        let mut offset_in_block = offset % 512;

        let mut total_bytes_read = 0;
        for i in 0..(BLOCK_SIZE / 512) {
            let mut data = vec![0u8; 512];
            let current_block_id = start_block_id + i;

            device.read_blocks(current_block_id, &mut data);

            let bytes_to_copy = if total_bytes_read == 0 {
                512 - offset_in_block
            } else {
                512
            };

            let buf_start = total_bytes_read;
            let buf_end = buf_start + bytes_to_copy;
            buf[buf_start..buf_end]
                .copy_from_slice(&data[offset_in_block..(offset_in_block + bytes_to_copy)]);

            total_bytes_read += bytes_to_copy;
            offset_in_block = 0; // only the first block has an offset within the block
        }

        buf
    }

    fn write_offset(&self, offset: usize, buf: &[u8]) {
        // log::info!("write_offset: {:x?} buf_len{:x?}", offset, buf.len());
        let device = get_blk_device(self.device_id).unwrap();

        let start_block_id = offset / 512;
        let mut offset_in_block = offset % 512;

        assert_eq!(offset_in_block, 0);

        let bytes_to_write = buf.len();
        let mut total_bytes_written = 0;

        for i in 0..((bytes_to_write + 511) / 512) {
            // round up to cover partial blocks
            let current_block_id = start_block_id + i;
            let mut data = [0u8; 512];

            if bytes_to_write < 512 {
                // Read the current block data first if we're writing less than a full block
                device.read_blocks(current_block_id, &mut data);
            }

            let buf_start = total_bytes_written;
            let buf_end = if buf_start + 512 > bytes_to_write {
                bytes_to_write
            } else {
                buf_start + 512
            };
            let bytes_to_copy = buf_end - buf_start;

            data[offset_in_block..offset_in_block + bytes_to_copy]
                .copy_from_slice(&buf[buf_start..buf_end]);
            device.write_blocks(current_block_id as usize, &data);

            total_bytes_written += bytes_to_copy;
            offset_in_block = 0; // only the first block has an offset within the block
        }
    }
}

pub struct Ext4FileSystem {
    inner: Arc<Ext4>,
    root: Arc<dyn INodeInterface>,
}

impl FileSystem for Ext4FileSystem {
    fn root_dir(&'static self) -> Arc<dyn INodeInterface> {
        self.root.clone()
    }

    fn name(&self) -> &str {
        "ext4"
    }
}

unsafe impl Sync for Ext4FileSystem {}
unsafe impl Send for Ext4FileSystem {}

impl Ext4FileSystem {
    pub fn new(device_id: usize) -> Arc<Self> {
        let disk = Arc::new(Ext4Disk::new(device_id));
        let ext4 = Ext4::open(disk);

        let root = Arc::new(Ext4FileWrapper::load_root(ext4.clone()));
        Arc::new(Self {
            inner: ext4,
            root: root,
        })
    }
}

pub struct Ext4FileWrapper {
    inner: Mutex<Ext4File>,
    ext4: Arc<Ext4>,
}

impl Ext4FileWrapper {
    fn load_root(ext4: Arc<Ext4>) -> Self {
        let mut ext4_file = Ext4File::new();
        let r = ext4.ext4_open(&mut ext4_file, "/", "r", false);

        Self {
            inner: Mutex::new(ext4_file),
            ext4: ext4,
        }
    }
}

impl INodeInterface for Ext4FileWrapper {
    fn open(&self, path: &str, _flags: vfscore::OpenFlags) -> VfsResult<Arc<dyn INodeInterface>> {
        let mut ext4_file = Ext4File::new();

        let mut create = false;

        if _flags.contains(OpenFlags::O_CREAT) {
            create = true;
        };

        let mut parse_flags: &str;
        match _flags {
            OpenFlags::O_RDONLY => parse_flags = "r",
            OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_TRUNC => parse_flags = "w",
            OpenFlags::O_WRONLY | OpenFlags::O_CREAT | OpenFlags::O_APPEND => parse_flags = "a",
            OpenFlags::O_RDWR => parse_flags = "r+",
            OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC => parse_flags = "w+",
            OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_APPEND => parse_flags = "a+",
            _ => unreachable!(),
        };

        let r = self
            .ext4
            .ext4_open(&mut ext4_file, path, parse_flags, create);

        if let Err(e) = r {
            match e.error() {
                Errnum::ENOENT => Err(vfscore::VfsError::FileNotFound),
                Errnum::EALLOCFIAL => Err(vfscore::VfsError::UnexpectedEof),
                Errnum::ELINKFIAL => Err(vfscore::VfsError::UnexpectedEof),

                _ => Err(vfscore::VfsError::UnexpectedEof),
            }
        } else {
            Ok(Arc::new(Ext4FileWrapper {
                inner: Mutex::new(ext4_file),
                ext4: self.ext4.clone(),
            }))
        }
    }

    fn mkdir(&self, path: &str) -> VfsResult<Arc<dyn INodeInterface>> {
        let r = self.ext4.ext4_dir_mk(path);

        let mut ext4_file = Ext4File::new();

        let r = self.ext4.ext4_open(&mut ext4_file, path, "w", false);

        Ok(Arc::new(Ext4FileWrapper {
            inner: Mutex::new(ext4_file),
            ext4: self.ext4.clone(),
        }))
    }

    fn metadata(&self) -> VfsResult<vfscore::Metadata> {
        todo!("ext4 loopup")
    }

    fn readat(&self, offset: usize, buffer: &mut [u8]) -> VfsResult<usize> {
        todo!("ext4 loopup")
    }

    fn writeat(&self, offset: usize, buffer: &[u8]) -> VfsResult<usize> {
        todo!("ext4 loopup")
    }

    fn rmdir(&self, name: &str) -> VfsResult<()> {
        todo!("ext4 loopup")
    }

    fn remove(&self, name: &str) -> VfsResult<()> {
        todo!("ext4 loopup")
    }

    fn touch(&self, path: &str) -> VfsResult<Arc<dyn INodeInterface>> {
        let mut ext4_file = Ext4File::new();
        let r = self.ext4.ext4_open(&mut ext4_file, path, "w+", true);
        Ok(Arc::new(Ext4FileWrapper {
            inner: Mutex::new(ext4_file),
            ext4: self.ext4.clone(),
        }))
    }

    fn read_dir(&self) -> VfsResult<Vec<DirEntry>> {
        let ext4file = self.inner.lock();
        let inode_num = ext4file.inode;

        let v: Vec<Ext4DirEntry> = self.ext4.read_dir_entry(inode_num as _);

        let mut entries = Vec::new();

        for i in v.iter() {
            let file_type = map_ext4_type(i.inner.file_type);

            let entry = DirEntry {
                name: i.get_name(),
                inode: i.inode,
                file_type: ile_type,
            };

            v.push(entry);
        }
        Ok(v)
    }

    fn lookup(&self, _name: &str) -> VfsResult<Arc<dyn INodeInterface>> {
        todo!("ext4 loopup")
    }

    fn truncate(&self, size: usize) -> VfsResult<()> {
        Ok(())
    }

    fn resolve_link(&self) -> VfsResult<alloc::string::String> {
        Err(vfscore::VfsError::NotSupported)
    }

    fn link(&self, _name: &str, _src: Arc<dyn INodeInterface>) -> VfsResult<()> {
        Err(vfscore::VfsError::NotSupported)
    }

    fn sym_link(&self, _name: &str, _src: &str) -> VfsResult<()> {
        Err(vfscore::VfsError::NotSupported)
    }

    fn unlink(&self, name: &str) -> VfsResult<()> {
        Ok(())
    }

    fn stat(&self, stat: &mut vfscore::Stat) -> VfsResult<()> {
        Ok(())
        // Err(vfscore::VfsError::NotSupported)
    }

    fn statfs(&self, statfs: &mut StatFS) -> VfsResult<()> {
        Ok(())
    }

    fn utimes(&self, _times: &mut [TimeSpec]) -> VfsResult<()> {
        Ok(())
    }
}

pub fn map_ext4_type(value: u8) -> FileType {
    match value {
        DirEntryType::EXT4_DE_REG_FILE => FileType::File,
        DirEntryType::EXT4_DE_DIR => FileType::Directory,
        DirEntryType::EXT4_DE_CHRDEV => FileType::File,
        DirEntryType::EXT4_DE_BLKDEV => FileType::File,
        DirEntryType::EXT4_DE_SOCK => FileType::File,
        DirEntryType::EXT4_DE_SYMLINK => FileType::LINK,
    }
}
