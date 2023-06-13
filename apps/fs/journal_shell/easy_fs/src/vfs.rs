#[cfg(feature = "journal")]
use crate::journal::get_buffer_dyn;

use super::{
    get_block_cache, BlockDevice, DirEntry, DiskInode, DiskInodeType, EasyFileSystem, DIRENT_SZ,
};
use alloc::string::String;
use alloc::vec::Vec;
use alloc::{rc::Rc, sync::Arc};
use axfs_vfs::{
    path::canonicalize, VfsDirEntry, VfsError, VfsNodeAttr, VfsNodeOps, VfsNodePerm, VfsNodeRef,
    VfsNodeType, VfsResult,
};
use spin::{Mutex, MutexGuard};
/// Virtual filesystem layer over easy-fs
pub struct Inode {
    inode_id: u32,
    block_id: usize,
    block_offset: usize,
    fs: Arc<Mutex<EasyFileSystem>>,
    block_device: Rc<dyn BlockDevice>,
}

unsafe impl Sync for Inode {}
unsafe impl Send for Inode {}

impl VfsNodeOps for Inode {
    fn get_attr(&self) -> VfsResult<VfsNodeAttr> {
        let perm = VfsNodePerm::from_bits_truncate(0o755);
        let (type_, size, blocks) = self.read_disk_inode(|disk_inode| {
            (
                if disk_inode.type_() == DiskInodeType::File {
                    VfsNodeType::File
                } else {
                    VfsNodeType::Dir
                },
                disk_inode.size as u64,
                disk_inode.data_blocks() as u64,
            )
        });
        Ok(VfsNodeAttr::new(perm, type_, size, blocks))
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            if disk_inode.type_() != DiskInodeType::File {
                Err(VfsError::IsADirectory)
            } else {
                Ok(disk_inode.read_at(offset as usize, buf, &self.block_device))
            }
        })
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> VfsResult<usize> {
        let offset = offset as usize;
        let mut fs = self.fs.lock();
        let res = self.modify_disk_inode(|disk_inode| {
            if disk_inode.type_() != DiskInodeType::File {
                Err(VfsError::IsADirectory)
            } else {
                #[cfg(feature = "journal")]
                let handle_rc = fs.journal_start(128).unwrap();
                #[cfg(feature = "journal")]
                let mut handle = handle_rc.as_ref().borrow_mut();
                self.increase_size(
                    (offset + buf.len()) as u32,
                    disk_inode,
                    &mut fs,
                    #[cfg(feature = "journal")]
                    (self.block_id as u32),
                    #[cfg(feature = "journal")]
                    &mut handle,
                );
                let size = disk_inode.write_at(
                    offset,
                    buf,
                    &self.block_device,
                    #[cfg(feature = "journal")]
                    &mut handle,
                    #[cfg(feature = "journal")]
                    false,
                );

                #[cfg(feature = "journal")]
                handle.stop().unwrap();
                #[cfg(feature = "journal")]
                fs.journal_commit();

                Ok(size)
            }
        })?;

        Ok(res)
    }

    fn truncate(&self, size: u64) -> VfsResult<()> {
        let mut fs = self.fs.lock();

        #[cfg(feature = "journal")]
        let handle_rc = fs.journal_start(128).unwrap();
        #[cfg(feature = "journal")]
        let mut handle = handle_rc.as_ref().borrow_mut();

        self.modify_disk_inode(|disk_inode| {
            self.increase_size(
                size as u32,
                disk_inode,
                &mut fs,
                #[cfg(feature = "journal")]
                (self.block_id as u32),
                #[cfg(feature = "journal")]
                &mut handle,
            );
            self.decrease_size(
                size as u32,
                disk_inode,
                &mut fs,
                #[cfg(feature = "journal")]
                (self.block_id as u32),
                #[cfg(feature = "journal")]
                &mut handle,
            )
        });

        #[cfg(feature = "journal")]
        handle.stop().unwrap();
        #[cfg(feature = "journal")]
        fs.journal_commit();
        Ok(())
    }

    fn parent(&self) -> Option<VfsNodeRef> {
        None
    }

    fn lookup(self: Arc<Self>, _path: &str) -> VfsResult<VfsNodeRef> {
        self.read_disk_inode(|disk_inode| {
            if disk_inode.type_() != DiskInodeType::Directory {
                Err(VfsError::NotADirectory)
            } else {
                Ok(())
            }
        })?;

        let components = path_components(_path);
        match components.len() {
            0 => Ok(self as VfsNodeRef),
            1 => self
                .find(&components[0])
                .map_or_else(|| Err(VfsError::NotFound), |x| Ok(x as VfsNodeRef)),
            _ => Err(VfsError::NotFound),
        }
    }

    fn create(&self, path: &str, ty: VfsNodeType) -> VfsResult {
        axlog::warn!("create: path: {}", path);
        if ty == VfsNodeType::Dir {
            return Err(VfsError::Unsupported);
        }
        let components = path_components(path);
        if components.is_empty() {
            return Err(VfsError::AlreadyExists);
        } else if components.len() > 1 {
            return Err(VfsError::Unsupported);
        }

        let name = &components[0];

        let mut fs = self.fs.lock();
        let op = |root_inode: &DiskInode| {
            // assert it is a directory
            assert!(root_inode.is_dir());
            // has the file been created?
            self.find_inode_id(name, root_inode)
        };
        if self.read_disk_inode(op).is_some() {
            return Err(VfsError::AlreadyExists);
        }
        // create a new file
        // alloc a inode with an indirect block
        #[cfg(feature = "journal")]
        let handle_rc = fs.journal_start(128).unwrap();
        #[cfg(feature = "journal")]
        let mut handle = handle_rc.as_ref().borrow_mut();
        let new_inode_id = fs.alloc_inode(
            #[cfg(feature = "journal")]
            Some(&mut handle),
        );
        // initialize inode
        let (new_inode_block_id, new_inode_block_offset) = fs.get_disk_inode_pos(new_inode_id);
        #[cfg(feature = "journal")]
        let buf = get_buffer_dyn(&self.block_device, new_inode_block_id as usize).unwrap();
        #[cfg(feature = "journal")]
        handle.get_write_access(&buf).unwrap();

        get_block_cache(new_inode_block_id as usize, Rc::clone(&self.block_device)).modify(
            new_inode_block_offset,
            |new_inode: &mut DiskInode| {
                new_inode.initialize(DiskInodeType::File);
            },
        );

        #[cfg(feature = "journal")]
        handle.dirty_metadata(&buf).unwrap();

        #[cfg(feature = "journal")]
        let self_buf = get_buffer_dyn(&self.block_device, self.block_id).unwrap();
        #[cfg(feature = "journal")]
        handle.get_write_access(&self_buf).unwrap();

        self.modify_disk_inode(|root_inode| {
            // append file in the dirent
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            // increase size
            self.increase_size(
                new_size as u32,
                root_inode,
                &mut fs,
                #[cfg(feature = "journal")]
                (self.block_id as u32),
                #[cfg(feature = "journal")]
                &mut handle,
            );
            // write dirent
            let dirent = DirEntry::new(name, new_inode_id);

            root_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
                #[cfg(feature = "journal")]
                &mut handle,
                #[cfg(feature = "journal")]
                true,
            );
        });

        #[cfg(feature = "journal")]
        handle.dirty_metadata(&self_buf).unwrap();

        #[cfg(feature = "journal")]
        handle.stop().unwrap();
        #[cfg(feature = "journal")]
        fs.journal_commit();

        Ok(())
    }

    fn remove(&self, _path: &str, _recursive: bool) -> VfsResult {
        let components = path_components(_path);
        match components.len() {
            1 => {
                self.unlink(&components[0]);
                Ok(())
            }
            _ => Err(VfsError::Unsupported),
        }
    }

    fn read_dir(
        &self,
        start_idx: usize,
        dirents: &mut [axfs_vfs::VfsDirEntry],
    ) -> VfsResult<usize> {
        let _fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            let file_count = (disk_inode.size as usize) / DIRENT_SZ;
            for i in start_idx..file_count {
                if i - start_idx >= dirents.len() {
                    return Ok(i - start_idx);
                }
                let mut dirent = DirEntry::empty();
                assert_eq!(
                    disk_inode.read_at(i * DIRENT_SZ, dirent.as_bytes_mut(), &self.block_device),
                    DIRENT_SZ,
                );
                dirents[i - start_idx] = VfsDirEntry::new(dirent.name(), VfsNodeType::File);
            }
            Ok(file_count - start_idx)
        })
    }
}

impl Inode {
    /// Create a vfs inode
    pub fn new(
        inode_id: u32,
        block_id: u32,
        block_offset: usize,
        fs: Arc<Mutex<EasyFileSystem>>,
        block_device: Rc<dyn BlockDevice>,
    ) -> Self {
        Self {
            inode_id,
            block_id: block_id as usize,
            block_offset,
            fs,
            block_device,
        }
    }
    /// Get related stats
    pub fn stat(&self) -> (u32, u32, DiskInodeType) {
        self.read_disk_inode(|disk_inode| {
            (self.inode_id, disk_inode.link_count, disk_inode.type_())
        })
    }
    /// Call a function over a disk inode to read it
    fn read_disk_inode<V>(&self, f: impl FnOnce(&DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Rc::clone(&self.block_device)).read(self.block_offset, f)
    }
    /// Call a function over a disk inode to modify it
    fn modify_disk_inode<V>(&self, f: impl FnOnce(&mut DiskInode) -> V) -> V {
        get_block_cache(self.block_id, Rc::clone(&self.block_device)).modify(self.block_offset, f)
    }
    /// Find inode under a disk inode by name
    fn find_inode_id(&self, name: &str, disk_inode: &DiskInode) -> Option<u32> {
        // assert it is a directory
        assert!(disk_inode.is_dir());
        let file_count = (disk_inode.size as usize) / DIRENT_SZ;
        let mut dirent = DirEntry::empty();
        for i in 0..file_count {
            assert_eq!(
                disk_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device),
                DIRENT_SZ,
            );
            if dirent.name() == name {
                return Some(dirent.inode_id());
            }
        }
        None
    }
    /// Find inode under current inode by name
    fn find(&self, name: &str) -> Option<Arc<Inode>> {
        let fs = self.fs.lock();
        self.read_disk_inode(|disk_inode| {
            self.find_inode_id(name, disk_inode)
                .map(|inode_id| self.get_inode(inode_id, &fs))
        })
    }

    fn get_inode(&self, inode_id: u32, fs: &MutexGuard<EasyFileSystem>) -> Arc<Inode> {
        let (block_id, block_offset) = fs.get_disk_inode_pos(inode_id);
        Arc::new(Self::new(
            inode_id,
            block_id,
            block_offset,
            self.fs.clone(),
            self.block_device.clone(),
        ))
    }

    /// Increase the size of a disk inode
    fn increase_size(
        &self,
        new_size: u32,
        disk_inode: &mut DiskInode,
        fs: &mut MutexGuard<EasyFileSystem>,
        #[cfg(feature = "journal")] disk_inode_block_id: u32,
        #[cfg(feature = "journal")] handle: &mut jbd::Handle,
    ) {
        if new_size <= disk_inode.size {
            return;
        }
        let blocks_needed = disk_inode.blocks_num_needed(new_size);
        let mut v: Vec<u32> = Vec::new();

        for _ in 0..blocks_needed {
            v.push(fs.alloc_data(
                #[cfg(feature = "journal")]
                Some(handle),
            ));
        }
        disk_inode.increase_size(
            new_size,
            v,
            &self.block_device,
            #[cfg(feature = "journal")]
            disk_inode_block_id,
            #[cfg(feature = "journal")]
            handle,
        );
    }

    /// Decrease the size of a disk node
    fn decrease_size(
        &self,
        new_size: u32,
        disk_inode: &mut DiskInode,
        fs: &mut MutexGuard<EasyFileSystem>,
        #[cfg(feature = "journal")] disk_inode_block_id: u32,
        #[cfg(feature = "journal")] handle: &mut jbd::Handle,
    ) {
        if new_size >= disk_inode.size {
            return;
        }

        let blocks_to_free = disk_inode.decrease_size(
            new_size,
            &self.block_device,
            #[cfg(feature = "journal")]
            disk_inode_block_id,
            #[cfg(feature = "journal")]
            handle,
        );
        for block_id in blocks_to_free {
            fs.dealloc_data(
                block_id,
                #[cfg(feature = "journal")]
                handle,
            );
        }
    }

    /// Create a hard link
    pub fn link(&self, name: &str, target: &str) -> Option<()> {
        let mut fs = self.fs.lock();

        let (inode_id, target_inode_id) = self.read_disk_inode(|root_inode: &DiskInode| {
            assert!(root_inode.is_dir());
            (
                self.find_inode_id(name, root_inode),
                self.find_inode_id(target, root_inode),
            )
        });

        if target_inode_id.is_none() || inode_id.is_some() {
            return None;
        }
        let target_inode_id = target_inode_id.unwrap();
        let target_inode = self.get_inode(target_inode_id, &fs);

        // Update the link count
        target_inode.modify_disk_inode(|disk_inode| {
            disk_inode.link_count += 1;
        });

        // Add an entry in the directory
        #[cfg(feature = "journal")]
        let handle_rc = fs.journal_start(128).unwrap();
        #[cfg(feature = "journal")]
        let mut handle = handle_rc.as_ref().borrow_mut();

        self.modify_disk_inode(|root_inode| {
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let new_size = (file_count + 1) * DIRENT_SZ;
            self.increase_size(
                new_size as u32,
                root_inode,
                &mut fs,
                #[cfg(feature = "journal")]
                (self.block_id as u32),
                #[cfg(feature = "journal")]
                &mut handle,
            );
            let dirent = DirEntry::new(name, target_inode_id);
            root_inode.write_at(
                file_count * DIRENT_SZ,
                dirent.as_bytes(),
                &self.block_device,
                #[cfg(feature = "journal")]
                &mut handle,
                #[cfg(feature = "journal")]
                true,
            );
        });

        #[cfg(feature = "journal")]
        handle.stop().unwrap();
        #[cfg(feature = "journal")]
        fs.journal_commit();

        Some(())
    }

    ///
    pub fn unlink(&self, name: &str) -> Option<()> {
        let mut fs = self.fs.lock();

        let file_disk_inode_id = self.read_disk_inode(|root_inode: &DiskInode| {
            // assert it is a directory
            assert!(root_inode.is_dir());
            // has the file been created?
            self.find_inode_id(name, root_inode)
        })?;

        let file_inode = self.get_inode(file_disk_inode_id, &fs);

        // Remove from the directory
        #[cfg(feature = "journal")]
        let handle_rc = fs.journal_start(128).unwrap();
        #[cfg(feature = "journal")]
        let mut handle = handle_rc.as_ref().borrow_mut();

        self.modify_disk_inode(|root_inode| {
            let file_count = (root_inode.size as usize) / DIRENT_SZ;
            let mut dirent = DirEntry::empty();

            for i in 0..file_count {
                assert_eq!(
                    root_inode.read_at(DIRENT_SZ * i, dirent.as_bytes_mut(), &self.block_device,),
                    DIRENT_SZ,
                );
                if dirent.name() == name {
                    // Remove the entry by moving all entries after it forward
                    for j in i..file_count - 1 {
                        assert_eq!(
                            root_inode.read_at(
                                DIRENT_SZ * (j + 1),
                                dirent.as_bytes_mut(),
                                &self.block_device,
                            ),
                            DIRENT_SZ,
                        );
                        root_inode.write_at(
                            DIRENT_SZ * j,
                            dirent.as_bytes(),
                            &self.block_device,
                            #[cfg(feature = "journal")]
                            &mut handle,
                            #[cfg(feature = "journal")]
                            true,
                        );
                    }
                    // Update the size
                    let new_size = (file_count - 1) * DIRENT_SZ;
                    self.decrease_size(
                        new_size as u32,
                        root_inode,
                        &mut fs,
                        #[cfg(feature = "journal")]
                        (self.block_id as u32),
                        #[cfg(feature = "journal")]
                        &mut handle,
                    );
                    break;
                }
            }
        });

        drop(fs);

        // Update the link count
        #[cfg(feature = "journal")]
        let buf = get_buffer_dyn(&file_inode.block_device, file_inode.block_id).unwrap();
        #[cfg(feature = "journal")]
        handle.get_write_access(&buf).unwrap();

        let new_link_count = file_inode.modify_disk_inode(|disk_inode| {
            disk_inode.link_count -= 1;
            disk_inode.link_count
        });
        #[cfg(feature = "journal")]
        handle.dirty_metadata(&buf).unwrap();

        // Remove the data blocks if the link count is 0
        if new_link_count == 0 {
            file_inode.clear(
                #[cfg(feature = "journal")]
                &mut handle,
            );
        }

        #[cfg(feature = "journal")]
        handle.stop().unwrap();
        #[cfg(feature = "journal")]
        self.fs.lock().journal_commit();

        Some(())
    }

    /// Clear the data in current inode
    pub fn clear(&self, #[cfg(feature = "journal")] handle: &mut jbd::Handle) {
        let mut fs = self.fs.lock();
        self.modify_disk_inode(|disk_inode| {
            let size = disk_inode.size;
            let data_blocks_dealloc = disk_inode.clear_size(
                &self.block_device,
                #[cfg(feature = "journal")]
                (self.block_id as u32),
                #[cfg(feature = "journal")]
                handle,
            );
            assert!(data_blocks_dealloc.len() == DiskInode::total_blocks(size) as usize);
            for data_block in data_blocks_dealloc.into_iter() {
                fs.dealloc_data(
                    data_block,
                    #[cfg(feature = "journal")]
                    handle,
                );
            }
        });
    }
}

fn path_components(path: &str) -> Vec<String> {
    if path.trim() == "" {
        return Vec::new();
    }
    let path = canonicalize(path);
    let mut trimmed_path = path.trim_matches('/');
    trimmed_path = path.strip_prefix("./").unwrap_or(trimmed_path);
    trimmed_path.split('/').map(String::from).collect()
}