//! Minimal original ext (pre-ext2) filesystem image writer.
//!
//! Layout (1024-byte blocks):
//!
//! ```text
//! block 0:                 boot
//! block 1:                 superblock
//! blocks 2..firstdz:       inode table (16 inodes per block)
//! blocks firstdz..:        data
//! ```
//!
//! Free space is tracked as in-band chains rather than bitmaps:
//!
//! - Free blocks: each chain block holds 254 block numbers + a `next`
//!   pointer to the previous chain block. Chain head in `s_firstfreeblock`.
//! - Free inodes: each chain block (overlaid on an unallocated inode slot)
//!   holds 14 inode numbers + a `next` pointer. Chain head in
//!   `s_firstfreeinode`.
//!
//! Files use 9 direct zone pointers plus one single-indirect (256 entries
//! at 4 bytes each). Maximum file size is therefore (9 + 256) × 1024 ≈
//! 265 KB.

use anyhow::Context;
#[cfg(unix)]
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const BLOCK_SIZE: usize = 1024;
const MAX_IMAGE_MB: u32 = 512;
pub const MAGIC: u16 = 0x137D;
const ROOT_INO: u32 = 1;
const NAME_LEN: usize = 255;
const DIR_PAD: u16 = 8;
const INODES_PER_BLOCK: u32 = (BLOCK_SIZE / 64) as u32; // 16
const PTRS_PER_BLOCK: u32 = (BLOCK_SIZE / 4) as u32; // 256
const DIRECT_BLOCKS: usize = 9;
const MAX_FILE_SIZE: usize = (DIRECT_BLOCKS + PTRS_PER_BLOCK as usize) * BLOCK_SIZE;
const INODE_SIZE: usize = 64;
const FREE_BLOCK_ENTRIES: usize = 254;
const FREE_INODE_ENTRIES: usize = 14;

#[cfg_attr(unix, allow(dead_code))]
const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// Stats reported after writing.
pub struct Stats {
    pub total_blocks: u32,
    pub ninodes: u32,
    pub first_data_zone: u32,
    pub free_blocks: u32,
    pub free_inodes: u32,
}

#[derive(Clone, Copy)]
struct Inode {
    mode: u16,
    uid: u16,
    size: u32,
    time: u32,
    gid: u16,
    nlinks: u16,
    /// 9 direct + 1 indirect + 1 double + 1 triple. This writer uses
    /// only direct (`zone[0..9]`) and single-indirect (`zone[9]`).
    zone: [u32; 12],
}

impl Inode {
    const fn zero() -> Self {
        Self {
            mode: 0,
            uid: 0,
            size: 0,
            time: 0,
            gid: 0,
            nlinks: 0,
            zone: [0; 12],
        }
    }
}

fn dir_rec_len(name_len: u16) -> u16 {
    (8 + name_len).div_ceil(DIR_PAD) * DIR_PAD
}

/// One free-inode chain block, overlaid on an unallocated inode slot.
struct FreeInodeChain {
    head_ino: u32,
    count: u32,
    free: [u32; FREE_INODE_ENTRIES],
    next: u32,
}

pub struct Ext {
    image: Vec<u8>,
    total_blocks: u32,
    ninodes: u32,
    first_data_zone: u32,
    inodes: Vec<Inode>,
    /// `inode_used[i]` set when inode `i` is allocated; index 0 unused.
    inode_used: Vec<bool>,
    /// `block_used[b]` set when block `b` is allocated.
    block_used: Vec<bool>,
    next_block: u32,
    next_inode: u32,
    #[cfg(unix)]
    source_inodes: HashMap<(u64, u64), u32>,
}

impl Ext {
    pub fn new(size_mb: u32) -> anyhow::Result<Self> {
        let image_len = checked_image_len(size_mb)?;
        let bytes = image_len as u64;
        if bytes / BLOCK_SIZE as u64 > u64::from(u32::MAX) {
            anyhow::bail!("filesystem exceeds 32-bit block-number limit");
        }
        let total_blocks = (bytes / BLOCK_SIZE as u64) as u32;
        if total_blocks < 16 {
            anyhow::bail!("filesystem too small (need at least 16 blocks)");
        }

        // 1 inode per 4 KB of filesystem, rounded up to a multiple of 16.
        let mut ninodes = total_blocks / 4;
        if ninodes % INODES_PER_BLOCK != 0 {
            ninodes = (ninodes / INODES_PER_BLOCK + 1) * INODES_PER_BLOCK;
        }
        let inode_blocks = ninodes / INODES_PER_BLOCK;
        let first_data_zone = 2 + inode_blocks;

        if total_blocks <= first_data_zone + 1 {
            anyhow::bail!("filesystem too small for layout");
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);

        let mut block_used = try_filled_vec(total_blocks as usize, false)?;
        for slot in block_used.iter_mut().take(first_data_zone as usize) {
            *slot = true;
        }

        let mut inode_used = try_filled_vec((ninodes + 1) as usize, false)?;
        inode_used[ROOT_INO as usize] = true;

        let mut ext = Self {
            image: try_zeroed_image(image_len)?,
            total_blocks,
            ninodes,
            first_data_zone,
            inodes: try_filled_vec(ninodes as usize, Inode::zero())?,
            inode_used,
            block_used,
            next_block: first_data_zone,
            next_inode: ROOT_INO + 1,
            #[cfg(unix)]
            source_inodes: HashMap::new(),
        };

        // Set up root inode and its initial directory block.
        let root_block = ext.alloc_block()?;
        {
            let root = &mut ext.inodes[(ROOT_INO - 1) as usize];
            root.mode = (S_IFDIR | 0o755) as u16;
            root.nlinks = 2;
            root.time = now;
            root.size = BLOCK_SIZE as u32;
            root.zone[0] = root_block;
        }
        ext.init_dir_block(root_block, ROOT_INO, ROOT_INO);

        Ok(ext)
    }

    pub fn populate_root(&mut self, src: &Path) -> anyhow::Result<()> {
        #[cfg(unix)]
        let dir = File::from(
            rustix::fs::open(
                src,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .with_context(|| format!("opening source directory {}", src.display()))?,
        );
        self.populate_dir(
            ROOT_INO,
            src,
            #[cfg(unix)]
            dir,
        )
    }

    fn populate_dir(
        &mut self,
        dir_ino: u32,
        src_path: &Path,
        #[cfg(unix)] dir: File,
    ) -> anyhow::Result<()> {
        #[cfg(unix)]
        let meta = dir
            .metadata()
            .with_context(|| format!("reading source directory metadata {}", src_path.display()))?;
        #[cfg(not(unix))]
        let meta = fs::metadata(src_path)?;
        self.inodes[(dir_ino - 1) as usize].time = ext_timestamp(meta.modified()?)?;
        #[cfg(unix)]
        let mut entries = rustix::fs::Dir::new(dir)
            .with_context(|| format!("reading source directory {}", src_path.display()))?;
        #[cfg(not(unix))]
        let mut entries = fs::read_dir(src_path)?;
        // Unix child opens borrow the stream's fd between iterator steps.
        #[cfg_attr(not(unix), allow(clippy::while_let_on_iterator))]
        while let Some(entry) = entries.next() {
            let entry = entry
                .with_context(|| format!("reading source directory {}", src_path.display()))?;
            #[cfg(unix)]
            let raw_name = {
                use std::os::unix::ffi::OsStrExt;
                let name = entry.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                std::ffi::OsStr::from_bytes(name).to_os_string()
            };
            #[cfg(not(unix))]
            let raw_name = entry.file_name();
            let path = src_path.join(&raw_name);
            #[cfg(unix)]
            let source = open_source_entry(entries.fd()?, &raw_name)
                .with_context(|| format!("importing {}", path.display()))?;
            #[cfg(unix)]
            let meta = source.metadata()?;
            #[cfg(not(unix))]
            let meta = fs::symlink_metadata(&path)?;
            #[cfg(unix)]
            let name = {
                use std::os::unix::ffi::OsStrExt;
                raw_name.as_bytes()
            };
            #[cfg(not(unix))]
            let name = raw_name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 filename: {:?}", raw_name))?
                .as_bytes();

            if name.len() > NAME_LEN {
                anyhow::bail!("name too long (max {NAME_LEN}): {:?}", raw_name);
            }
            if !meta.is_file() && !meta.is_dir() {
                anyhow::bail!("unsupported file type: {}", path.display());
            }

            #[cfg(unix)]
            let source_id = {
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino())
            };
            #[cfg(unix)]
            if meta.is_file() {
                if let Some(&ino) = self.source_inodes.get(&source_id) {
                    let links = self.inodes[(ino - 1) as usize]
                        .nlinks
                        .checked_add(1)
                        .ok_or_else(|| {
                            anyhow::anyhow!("too many hard links: {}", path.display())
                        })?;
                    self.add_dir_entry(dir_ino, name, ino)?;
                    self.inodes[(ino - 1) as usize].nlinks = links;
                    continue;
                }
            }

            let child_ino = self.alloc_inode()?;
            {
                let child = &mut self.inodes[(child_ino - 1) as usize];
                child.mode = mode_from_meta(&meta) as u16;
                (child.uid, child.gid) = ownership_from_meta(&meta)?;
                child.time = ext_timestamp(meta.modified()?)?;
            }

            if meta.is_dir() {
                let block = self.alloc_block()?;
                {
                    let child = &mut self.inodes[(child_ino - 1) as usize];
                    child.zone[0] = block;
                    child.nlinks = 2;
                    child.size = BLOCK_SIZE as u32;
                }
                self.init_dir_block(block, child_ino, dir_ino);
                self.add_dir_entry(dir_ino, name, child_ino)?;
                self.inodes[(dir_ino - 1) as usize].nlinks += 1;
                self.populate_dir(
                    child_ino,
                    &path,
                    #[cfg(unix)]
                    source,
                )?;
            } else {
                self.inodes[(child_ino - 1) as usize].nlinks = 1;
                self.add_dir_entry(dir_ino, name, child_ino)?;
                #[cfg(not(unix))]
                let source = File::open(&path)?;
                self.write_file_data(child_ino, source, &path)?;
                #[cfg(unix)]
                self.source_inodes.insert(source_id, child_ino);
            }
        }
        Ok(())
    }

    fn write_file_data(&mut self, ino: u32, source: File, path: &Path) -> anyhow::Result<()> {
        let mut data = Vec::new();
        source
            .take(MAX_FILE_SIZE as u64 + 1)
            .read_to_end(&mut data)?;
        if data.len() > MAX_FILE_SIZE {
            anyhow::bail!(
                "file too large (max {MAX_FILE_SIZE} bytes): {}",
                path.display()
            );
        }
        self.inodes[(ino - 1) as usize].size = data.len() as u32;

        let mut indirect = [0u32; PTRS_PER_BLOCK as usize];
        let mut have_indirect = false;

        for (file_block, chunk) in data.chunks(BLOCK_SIZE).enumerate() {
            let disk_block = self.alloc_block()?;
            let off = (disk_block as usize) * BLOCK_SIZE;
            self.image[off..off + chunk.len()].copy_from_slice(chunk);

            if file_block < DIRECT_BLOCKS {
                self.inodes[(ino - 1) as usize].zone[file_block] = disk_block;
            } else {
                let idx = file_block - DIRECT_BLOCKS;
                if idx >= PTRS_PER_BLOCK as usize {
                    anyhow::bail!("file too large (needs double indirect)");
                }
                indirect[idx] = disk_block;
                have_indirect = true;
            }
        }

        if have_indirect {
            let ind_block = self.alloc_block()?;
            self.inodes[(ino - 1) as usize].zone[9] = ind_block;
            let off = (ind_block as usize) * BLOCK_SIZE;
            for (i, z) in indirect.iter().enumerate() {
                let p = off + i * 4;
                self.image[p..p + 4].copy_from_slice(&z.to_le_bytes());
            }
        }
        Ok(())
    }

    fn add_dir_entry(&mut self, dir_ino: u32, name: &[u8], child_ino: u32) -> anyhow::Result<()> {
        let name_bytes = name;
        let name_len = name_bytes.len() as u16;
        let need = dir_rec_len(name_len);

        let dir = self.inodes[(dir_ino - 1) as usize];
        let n_blocks = (dir.size as usize).div_ceil(BLOCK_SIZE);

        for b in 0..n_blocks {
            let disk_block = dir.zone[b];
            let block_off = (disk_block as usize) * BLOCK_SIZE;
            let mut off = 0usize;
            while off < BLOCK_SIZE {
                let de_off = block_off + off;
                let d_rec_len =
                    u16::from_le_bytes(self.image[de_off + 4..de_off + 6].try_into().unwrap());
                let d_name_len =
                    u16::from_le_bytes(self.image[de_off + 6..de_off + 8].try_into().unwrap());
                let actual_len = dir_rec_len(d_name_len);
                let slack = d_rec_len.saturating_sub(actual_len);

                if slack >= need {
                    // Shrink the existing entry and write the new one in the slack.
                    self.image[de_off + 4..de_off + 6].copy_from_slice(&actual_len.to_le_bytes());
                    let new_off = de_off + actual_len as usize;
                    self.image[new_off..new_off + 4].copy_from_slice(&child_ino.to_le_bytes());
                    self.image[new_off + 4..new_off + 6].copy_from_slice(&slack.to_le_bytes());
                    self.image[new_off + 6..new_off + 8].copy_from_slice(&name_len.to_le_bytes());
                    self.image[new_off + 8..new_off + 8 + name_bytes.len()]
                        .copy_from_slice(name_bytes);
                    return Ok(());
                }
                off += d_rec_len as usize;
            }
        }

        // No slack — allocate a new directory block.
        let block_idx = n_blocks;
        if block_idx >= DIRECT_BLOCKS {
            anyhow::bail!("directory too large");
        }
        let new_block = self.alloc_block()?;
        self.inodes[(dir_ino - 1) as usize].zone[block_idx] = new_block;
        self.inodes[(dir_ino - 1) as usize].size = ((block_idx + 1) * BLOCK_SIZE) as u32;

        let off = (new_block as usize) * BLOCK_SIZE;
        self.image[off..off + 4].copy_from_slice(&child_ino.to_le_bytes());
        self.image[off + 4..off + 6].copy_from_slice(&(BLOCK_SIZE as u16).to_le_bytes());
        self.image[off + 6..off + 8].copy_from_slice(&name_len.to_le_bytes());
        self.image[off + 8..off + 8 + name_bytes.len()].copy_from_slice(name_bytes);
        Ok(())
    }

    fn init_dir_block(&mut self, block: u32, self_ino: u32, parent_ino: u32) {
        let off = (block as usize) * BLOCK_SIZE;
        let dot_len = dir_rec_len(1);

        // dot entry
        self.image[off..off + 4].copy_from_slice(&self_ino.to_le_bytes());
        self.image[off + 4..off + 6].copy_from_slice(&dot_len.to_le_bytes());
        self.image[off + 6..off + 8].copy_from_slice(&1u16.to_le_bytes());
        self.image[off + 8] = b'.';

        // dotdot entry, fills the remainder of the block
        let dotdot_off = off + dot_len as usize;
        let dotdot_len = BLOCK_SIZE as u16 - dot_len;
        self.image[dotdot_off..dotdot_off + 4].copy_from_slice(&parent_ino.to_le_bytes());
        self.image[dotdot_off + 4..dotdot_off + 6].copy_from_slice(&dotdot_len.to_le_bytes());
        self.image[dotdot_off + 6..dotdot_off + 8].copy_from_slice(&2u16.to_le_bytes());
        self.image[dotdot_off + 8] = b'.';
        self.image[dotdot_off + 9] = b'.';
    }

    fn alloc_block(&mut self) -> anyhow::Result<u32> {
        while self.next_block < self.total_blocks {
            let b = self.next_block as usize;
            self.next_block += 1;
            if !self.block_used[b] {
                self.block_used[b] = true;
                return Ok(b as u32);
            }
        }
        anyhow::bail!("no free blocks");
    }

    fn alloc_inode(&mut self) -> anyhow::Result<u32> {
        while self.next_inode <= self.ninodes {
            let i = self.next_inode as usize;
            self.next_inode += 1;
            if !self.inode_used[i] {
                self.inode_used[i] = true;
                return Ok(i as u32);
            }
        }
        anyhow::bail!("no free inodes");
    }

    pub fn write_to(mut self, path: &Path) -> anyhow::Result<Stats> {
        let (first_free_block, free_blocks) = self.build_free_block_list();
        let (first_free_inode, free_inodes, free_chains) = self.build_free_inode_list();
        self.write_superblock(first_free_block, free_blocks, first_free_inode, free_inodes);
        self.write_inode_table(&free_chains);

        let mut f = File::create(path)?;
        f.write_all(&self.image)?;
        f.sync_all()?;

        Ok(Stats {
            total_blocks: self.total_blocks,
            ninodes: self.ninodes,
            first_data_zone: self.first_data_zone,
            free_blocks,
            free_inodes,
        })
    }

    /// Builds the on-disk free-block chain by walking unallocated blocks
    /// from end to start. Each chain block holds up to 254 free block
    /// numbers + a `next` pointer to the previously-built chain block. The
    /// chain block ITSELF is also a free block (consumed last when the
    /// kernel exhausts its `free[]` array), so it's marked used here and
    /// counted in the total.
    fn build_free_block_list(&mut self) -> (u32, u32) {
        let mut count_tot: u32 = 0;
        let mut next_chain: u32 = 0;
        let mut pending: u32 = 0;
        let mut efb_count: u32 = 0;
        let mut efb_free = [0u32; FREE_BLOCK_ENTRIES];

        for blk in (self.first_data_zone..self.total_blocks).rev() {
            if self.block_used[blk as usize] {
                continue;
            }
            if pending == 0 {
                pending = blk;
            } else {
                efb_free[efb_count as usize] = blk;
                efb_count += 1;
            }
            if efb_count == FREE_BLOCK_ENTRIES as u32 {
                self.write_efb(pending, efb_count, &efb_free, next_chain);
                self.block_used[pending as usize] = true;
                count_tot += efb_count + 1;
                next_chain = pending;
                pending = 0;
                efb_count = 0;
                efb_free = [0u32; FREE_BLOCK_ENTRIES];
            }
        }
        if pending != 0 {
            self.write_efb(pending, efb_count, &efb_free, next_chain);
            self.block_used[pending as usize] = true;
            count_tot += efb_count + 1;
            next_chain = pending;
        }
        (next_chain, count_tot)
    }

    fn write_efb(&mut self, block: u32, count: u32, free: &[u32; FREE_BLOCK_ENTRIES], next: u32) {
        let off = (block as usize) * BLOCK_SIZE;
        for byte in self.image[off..off + BLOCK_SIZE].iter_mut() {
            *byte = 0;
        }
        self.image[off..off + 4].copy_from_slice(&count.to_le_bytes());
        for (i, &f) in free.iter().enumerate() {
            let p = off + 4 + i * 4;
            self.image[p..p + 4].copy_from_slice(&f.to_le_bytes());
        }
        let next_off = off + 4 + FREE_BLOCK_ENTRIES * 4;
        self.image[next_off..next_off + 4].copy_from_slice(&next.to_le_bytes());
    }

    /// Builds the on-disk free-inode chain by walking unallocated inodes
    /// from highest to lowest. Each chain block (overlaid on an
    /// unallocated inode slot) holds up to 14 inode numbers + a `next`
    /// pointer. Returns (head, total_count, chain_blocks_to_overlay).
    fn build_free_inode_list(&self) -> (u32, u32, Vec<FreeInodeChain>) {
        let mut chains: Vec<FreeInodeChain> = Vec::new();
        let mut current: Option<FreeInodeChain> = None;
        let mut count_tot: u32 = 0;
        let mut next_chain: u32 = 0;
        let mut head: u32 = 0;

        for ino in (1..=self.ninodes).rev() {
            if self.inode_used[ino as usize] {
                continue;
            }
            if let Some(chain) = current.as_mut() {
                chain.free[chain.count as usize] = ino;
                chain.count += 1;
            } else {
                head = ino;
                current = Some(FreeInodeChain {
                    head_ino: ino,
                    count: 0,
                    free: [0; FREE_INODE_ENTRIES],
                    next: next_chain,
                });
                next_chain = ino;
            }
            if current
                .as_ref()
                .is_some_and(|c| c.count == FREE_INODE_ENTRIES as u32)
            {
                let c = current.take().unwrap();
                count_tot += c.count + 1;
                chains.push(c);
            }
        }
        if let Some(c) = current {
            count_tot += c.count + 1;
            chains.push(c);
        }
        (head, count_tot, chains)
    }

    fn write_superblock(
        &mut self,
        first_free_block: u32,
        free_blocks: u32,
        first_free_inode: u32,
        free_inodes: u32,
    ) {
        let max_size = (1u32 << 31) - 1;
        let sb_off = BLOCK_SIZE; // block 1
        let sb = &mut self.image[sb_off..sb_off + BLOCK_SIZE];
        for byte in sb.iter_mut() {
            *byte = 0;
        }
        sb[0..4].copy_from_slice(&self.ninodes.to_le_bytes()); // s_ninodes
        sb[4..8].copy_from_slice(&self.total_blocks.to_le_bytes()); // s_nzones
        sb[8..12].copy_from_slice(&first_free_block.to_le_bytes()); // s_firstfreeblock
        sb[12..16].copy_from_slice(&free_blocks.to_le_bytes()); // s_freeblockscount
        sb[16..20].copy_from_slice(&first_free_inode.to_le_bytes()); // s_firstfreeinode
        sb[20..24].copy_from_slice(&free_inodes.to_le_bytes()); // s_freeinodescount
        sb[24..28].copy_from_slice(&self.first_data_zone.to_le_bytes()); // s_firstdatazone
        sb[28..32].copy_from_slice(&0u32.to_le_bytes()); // s_log_zone_size
        sb[32..36].copy_from_slice(&max_size.to_le_bytes()); // s_max_size
                                                             // s_reserved[5] at 36..56 stays zero
        sb[56..58].copy_from_slice(&MAGIC.to_le_bytes()); // s_magic (u16)
    }

    fn write_inode_table(&mut self, free_chains: &[FreeInodeChain]) {
        let table_off = 2 * BLOCK_SIZE;

        for (idx, inode) in self.inodes.iter().enumerate() {
            let off = table_off + idx * INODE_SIZE;
            let buf = &mut self.image[off..off + INODE_SIZE];
            buf[0..2].copy_from_slice(&inode.mode.to_le_bytes());
            buf[2..4].copy_from_slice(&inode.uid.to_le_bytes());
            buf[4..8].copy_from_slice(&inode.size.to_le_bytes());
            buf[8..12].copy_from_slice(&inode.time.to_le_bytes());
            buf[12..14].copy_from_slice(&inode.gid.to_le_bytes());
            buf[14..16].copy_from_slice(&inode.nlinks.to_le_bytes());
            for (i, z) in inode.zone.iter().enumerate() {
                let p = 16 + i * 4;
                buf[p..p + 4].copy_from_slice(&z.to_le_bytes());
            }
        }

        // Overlay each free-inode chain block onto its head inode's slot.
        for chain in free_chains {
            let off = table_off + (chain.head_ino - 1) as usize * INODE_SIZE;
            let buf = &mut self.image[off..off + INODE_SIZE];
            for byte in buf.iter_mut() {
                *byte = 0;
            }
            buf[0..4].copy_from_slice(&chain.count.to_le_bytes());
            for (i, &f) in chain.free.iter().enumerate() {
                let p = 4 + i * 4;
                buf[p..p + 4].copy_from_slice(&f.to_le_bytes());
            }
            buf[60..64].copy_from_slice(&chain.next.to_le_bytes());
        }
    }
}

fn checked_image_len(size_mb: u32) -> anyhow::Result<usize> {
    if size_mb == 0 {
        anyhow::bail!("size must be positive");
    }
    let bytes = u64::from(size_mb) * 1024 * 1024;
    let len = usize::try_from(bytes)
        .map_err(|_| anyhow::anyhow!("image size exceeds the host address space"))?;
    if len > isize::MAX as usize {
        anyhow::bail!("image size exceeds the host allocation limit");
    }
    if size_mb > MAX_IMAGE_MB {
        anyhow::bail!("image too large (max {MAX_IMAGE_MB} MiB for this in-memory writer)");
    }
    Ok(len)
}

fn try_zeroed_image(len: usize) -> anyhow::Result<Vec<u8>> {
    use std::alloc::{alloc_zeroed, Layout};
    if len == 0 {
        return Ok(Vec::new());
    }
    let layout =
        Layout::array::<u8>(len).context("image size exceeds the host allocation limit")?;
    // SAFETY: layout has nonzero size. A null result is handled before
    // constructing the Vec. The global allocator supplies zero-initialized
    // bytes, and the Vec uses the same allocation size and alignment.
    let ptr = unsafe { alloc_zeroed(layout) };
    if ptr.is_null() {
        anyhow::bail!("not enough memory to allocate filesystem image");
    }
    // SAFETY: all len bytes are initialized, uniquely owned, and allocated
    // using the global allocator with the layout Vec<u8> will deallocate.
    Ok(unsafe { Vec::from_raw_parts(ptr, len, len) })
}

fn try_filled_vec<T: Clone>(len: usize, value: T) -> anyhow::Result<Vec<T>> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(len)
        .context("not enough memory to allocate filesystem image or metadata")?;
    values.resize(len, value);
    Ok(values)
}

// Resolve each child relative to a pinned directory, never through a path
// whose ancestors could have been replaced while walking the source tree.
#[cfg(unix)]
fn open_source_entry(dir: impl std::os::fd::AsFd, name: &std::ffi::OsStr) -> anyhow::Result<File> {
    use rustix::fs::{statat, AtFlags};
    let expected = statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)?;
    open_checked_source(dir, name, &expected)
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)] // stat field widths differ between Unix targets.
fn open_checked_source(
    dir: impl std::os::fd::AsFd,
    name: &std::ffi::OsStr,
    expected: &rustix::fs::Stat,
) -> anyhow::Result<File> {
    use rustix::fs::{openat, FileType, Mode, OFlags};
    use std::os::unix::fs::MetadataExt;
    let kind = FileType::from_raw_mode(expected.st_mode);
    if kind != FileType::RegularFile && kind != FileType::Directory {
        anyhow::bail!("unsupported file type: {name:?}");
    }
    let mut flags =
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY;
    if kind == FileType::Directory {
        flags |= OFlags::DIRECTORY;
    }
    let file = File::from(openat(dir, name, flags, Mode::empty())?);
    let actual = file.metadata()?;
    if actual.dev() != expected.st_dev as u64
        || actual.ino() != expected.st_ino as u64
        || (actual.is_file() != (kind == FileType::RegularFile))
        || (actual.is_dir() != (kind == FileType::Directory))
    {
        anyhow::bail!("source entry changed while opening: {name:?}");
    }
    Ok(file)
}

fn ext_timestamp(time: SystemTime) -> anyhow::Result<u32> {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_err(|_| anyhow::anyhow!("modification time precedes the Unix epoch"))?
        .as_secs();
    u32::try_from(seconds)
        .map_err(|_| anyhow::anyhow!("modification time exceeds ext's 32-bit range"))
}

fn checked_ownership(uid: u32, gid: u32) -> anyhow::Result<(u16, u16)> {
    Ok((
        u16::try_from(uid).map_err(|_| anyhow::anyhow!("UID {uid} exceeds ext's 16-bit range"))?,
        u16::try_from(gid).map_err(|_| anyhow::anyhow!("GID {gid} exceeds ext's 16-bit range"))?,
    ))
}

#[cfg(unix)]
fn ownership_from_meta(meta: &fs::Metadata) -> anyhow::Result<(u16, u16)> {
    use std::os::unix::fs::MetadataExt;
    checked_ownership(meta.uid(), meta.gid())
}

#[cfg(not(unix))]
fn ownership_from_meta(_meta: &fs::Metadata) -> anyhow::Result<(u16, u16)> {
    checked_ownership(0, 0)
}

#[cfg(unix)]
fn mode_from_meta(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(not(unix))]
fn mode_from_meta(meta: &fs::Metadata) -> u32 {
    if meta.is_dir() {
        S_IFDIR | 0o755
    } else {
        S_IFREG | 0o644
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn read_u16_le(buf: &[u8], off: usize) -> u16 {
        u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
    }
    fn read_u32_le(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    }

    #[test]
    fn empty_image_has_correct_superblock() {
        let dir = TempDir::new().unwrap();
        let img = dir.path().join("test.ext");
        Ext::new(2).unwrap().write_to(&img).unwrap();

        let bytes = fs::read(&img).unwrap();
        assert_eq!(bytes.len(), 2 * 1024 * 1024);
        let sb = BLOCK_SIZE;
        // s_nzones
        assert_eq!(read_u32_le(&bytes, sb + 4), 2048);
        // magic at offset 56 (u16)
        assert_eq!(read_u16_le(&bytes, sb + 56), MAGIC);
    }

    #[test]
    fn root_block_has_dot_and_dotdot() {
        let dir = TempDir::new().unwrap();
        let img = dir.path().join("test.ext");
        Ext::new(2).unwrap().write_to(&img).unwrap();

        let bytes = fs::read(&img).unwrap();
        // Inode 1 lives at byte 2*BLOCK_SIZE (start of inode table).
        // i_zone[0] is at offset 16.
        let root_off = 2 * BLOCK_SIZE;
        let root_block = read_u32_le(&bytes, root_off + 16);
        let dir_off = (root_block as usize) * BLOCK_SIZE;
        // dot entry: inode (4) + rec_len (2) + name_len (2) + name
        assert_eq!(read_u32_le(&bytes, dir_off), ROOT_INO);
        assert_eq!(read_u16_le(&bytes, dir_off + 6), 1);
        assert_eq!(bytes[dir_off + 8], b'.');
    }

    #[test]
    fn populate_walks_subdirs() {
        let src = TempDir::new().unwrap();
        let mut f = File::create(src.path().join("hello.txt")).unwrap();
        f.write_all(b"hi\n").unwrap();
        drop(f);
        fs::create_dir(src.path().join("sub")).unwrap();
        File::create(src.path().join("sub").join("nested.txt")).unwrap();

        let dst = TempDir::new().unwrap();
        let img = dst.path().join("out.ext");
        let mut ext = Ext::new(2).unwrap();
        ext.populate_root(src.path()).unwrap();
        let stats = ext.write_to(&img).unwrap();
        assert!(stats.free_blocks > 0);
        assert!(stats.free_inodes > 0);
    }

    #[test]
    fn ownership_range_is_checked() {
        assert_eq!(checked_ownership(65535, 65535).unwrap(), (65535, 65535));
        assert!(checked_ownership(65536, 0).is_err());
        assert!(checked_ownership(0, 65536).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn imported_setuid_file_keeps_its_owner() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let src = TempDir::new().unwrap();
        let path = src.path().join("file");
        fs::write(&path, b"data").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o6755)).unwrap();
        let meta = fs::metadata(&path).unwrap();
        let mut ext = Ext::new(1).unwrap();
        ext.populate_root(src.path()).unwrap();
        let dst = TempDir::new().unwrap();
        let image = dst.path().join("image");
        ext.write_to(&image).unwrap();
        let bytes = fs::read(image).unwrap();
        let off = 2 * BLOCK_SIZE + INODE_SIZE;
        assert_eq!(read_u16_le(&bytes, off), meta.mode() as u16);
        assert_eq!(u32::from(read_u16_le(&bytes, off + 2)), meta.uid());
        assert_eq!(u32::from(read_u16_le(&bytes, off + 12)), meta.gid());
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_entries_are_rejected() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        let src = TempDir::new().unwrap();
        let path = src.path().join("unsupported");
        symlink("missing", &path).unwrap();
        let mut ext = Ext::new(1).unwrap();
        assert!(ext
            .populate_root(src.path())
            .unwrap_err()
            .chain()
            .any(|cause| cause.to_string().contains("unsupported file type")));
        fs::remove_file(&path).unwrap();
        let _socket = UnixListener::bind(&path).unwrap();
        let mut ext = Ext::new(1).unwrap();
        assert!(ext
            .populate_root(src.path())
            .unwrap_err()
            .chain()
            .any(|cause| cause.to_string().contains("unsupported file type")));
    }

    fn find_entry(ext: &Ext, dir: u32, name: &[u8]) -> u32 {
        let inode = ext.inodes[(dir - 1) as usize];
        for &block in &inode.zone[..inode.size as usize / BLOCK_SIZE] {
            let mut off = block as usize * BLOCK_SIZE;
            let end = off + BLOCK_SIZE;
            while off < end {
                let len = read_u16_le(&ext.image, off + 6) as usize;
                if &ext.image[off + 8..off + 8 + len] == name {
                    return read_u32_le(&ext.image, off);
                }
                off += read_u16_le(&ext.image, off + 4) as usize;
            }
        }
        panic!("entry not found: {name:?}");
    }

    #[cfg(unix)]
    #[test]
    fn hard_links_share_data_and_count_only_imported_names() {
        let src = TempDir::new().unwrap();
        fs::create_dir(src.path().join("sub")).unwrap();
        let original = src.path().join("file");
        fs::write(&original, b"payload").unwrap();
        fs::hard_link(&original, src.path().join("sub/link")).unwrap();
        let outside = TempDir::new().unwrap();
        fs::hard_link(&original, outside.path().join("not-imported")).unwrap();
        let mut ext = Ext::new(1).unwrap();
        ext.populate_root(src.path()).unwrap();
        let file = find_entry(&ext, ROOT_INO, b"file");
        let sub = find_entry(&ext, ROOT_INO, b"sub");
        assert_eq!(file, find_entry(&ext, sub, b"link"));
        let inode = ext.inodes[(file - 1) as usize];
        assert_eq!(inode.nlinks, 2);
        assert_eq!(inode.size, 7);
        assert_eq!(ext.inode_used.iter().filter(|&&used| used).count(), 3);
        let off = inode.zone[0] as usize * BLOCK_SIZE;
        assert_eq!(&ext.image[off..off + 7], b"payload");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_names_are_preserved_in_directories() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let src = TempDir::new().unwrap();
        let name = OsStr::from_bytes(b"dir\xff");
        fs::create_dir(src.path().join(name)).unwrap();
        fs::write(
            src.path().join(name).join(OsStr::from_bytes(b"file\xfe")),
            b"bytes",
        )
        .unwrap();
        let mut ext = Ext::new(1).unwrap();
        ext.populate_root(src.path()).unwrap();
        let dir = find_entry(&ext, ROOT_INO, name.as_bytes());
        let file = find_entry(&ext, dir, b"file\xfe");
        assert_eq!(ext.inodes[(file - 1) as usize].size, 5);
    }

    #[test]
    fn file_size_limit_is_checked_before_allocating_blocks() {
        let src = TempDir::new().unwrap();
        let path = src.path().join("file");
        let file = File::create(&path).unwrap();
        for size in [MAX_FILE_SIZE as u64 + 1, 32 * 1024 * 1024] {
            file.set_len(size).unwrap();
            let mut ext = Ext::new(1).unwrap();
            let next_block = ext.next_block;
            let error = ext.populate_root(src.path()).unwrap_err();
            assert!(error.to_string().contains("file too large"));
            assert_eq!(ext.next_block, next_block);
        }
        file.set_len(MAX_FILE_SIZE as u64).unwrap();
        let mut ext = Ext::new(1).unwrap();
        ext.populate_root(src.path()).unwrap();
        let ino = find_entry(&ext, ROOT_INO, b"file");
        let inode = ext.inodes[(ino - 1) as usize];
        assert_eq!(inode.size as usize, MAX_FILE_SIZE);
        assert_ne!(inode.zone[9], 0);
        assert_ne!(
            read_u32_le(&ext.image, inode.zone[9] as usize * BLOCK_SIZE + 255 * 4),
            0
        );
    }

    #[test]
    fn imported_modification_times_are_preserved() {
        use std::fs::FileTimes;
        use std::time::Duration;
        let src = TempDir::new().unwrap();
        fs::create_dir(src.path().join("sub")).unwrap();
        let path = src.path().join("sub/file");
        let file = File::create(&path).unwrap();
        let seconds = 1_000_000_000;
        file.set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
            .unwrap();
        #[cfg(unix)]
        for (path, seconds) in [
            (src.path().to_path_buf(), 900_000_000),
            (src.path().join("sub"), 950_000_000),
        ] {
            File::open(path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(seconds)))
                .unwrap();
        }
        let root_time =
            ext_timestamp(fs::metadata(src.path()).unwrap().modified().unwrap()).unwrap();
        let sub_time = ext_timestamp(
            fs::metadata(src.path().join("sub"))
                .unwrap()
                .modified()
                .unwrap(),
        )
        .unwrap();
        let mut ext = Ext::new(1).unwrap();
        ext.populate_root(src.path()).unwrap();
        let sub = find_entry(&ext, ROOT_INO, b"sub");
        let ino = find_entry(&ext, sub, b"file");
        let dst = TempDir::new().unwrap();
        let image = dst.path().join("image");
        ext.write_to(&image).unwrap();
        let bytes = fs::read(image).unwrap();
        for (ino, expected) in [
            (ROOT_INO, root_time),
            (sub, sub_time),
            (ino, seconds as u32),
        ] {
            assert_eq!(
                read_u32_le(&bytes, 2 * BLOCK_SIZE + (ino as usize - 1) * INODE_SIZE + 8),
                expected
            );
        }
    }

    #[test]
    fn unrepresentable_modification_times_are_rejected() {
        use std::time::Duration;
        assert!(ext_timestamp(UNIX_EPOCH - Duration::from_secs(1)).is_err());
        assert_eq!(
            ext_timestamp(UNIX_EPOCH + Duration::from_secs(u32::MAX as u64)).unwrap(),
            u32::MAX
        );
        assert!(ext_timestamp(UNIX_EPOCH + Duration::from_secs(u32::MAX as u64 + 1)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn replaced_source_entries_are_rejected_without_reading_or_blocking() {
        use rustix::fs::{statat, AtFlags, FileType, Mode};
        use std::ffi::OsStr;
        use std::os::unix::fs::symlink;
        let src = TempDir::new().unwrap();
        let dir = File::open(src.path()).unwrap();
        let name = OsStr::new("file");
        let path = src.path().join(name);
        fs::write(&path, b"original").unwrap();
        let expected = statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW).unwrap();
        // Keep the original inode alive so its number cannot be reused.
        fs::rename(&path, src.path().join("original")).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert!(open_checked_source(&dir, name, &expected).is_err());
        fs::remove_file(&path).unwrap();
        symlink("original", &path).unwrap();
        assert!(open_checked_source(&dir, name, &expected).is_err());
        fs::remove_file(&path).unwrap();
        rustix::fs::mknodat(&dir, name, FileType::Fifo, Mode::RUSR | Mode::WUSR, 0).unwrap();
        // O_NONBLOCK makes opening the substituted FIFO return immediately.
        assert!(open_checked_source(&dir, name, &expected).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn source_handles_survive_path_replacement() {
        use std::ffi::OsStr;
        use std::os::unix::fs::symlink;
        let src = TempDir::new().unwrap();
        let path = src.path().join("source");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("file"), b"original").unwrap();
        let dir = File::open(&path).unwrap();
        let mut file = open_source_entry(&dir, OsStr::new("file")).unwrap();
        let outside = src.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), b"secret").unwrap();
        fs::rename(&path, src.path().join("moved")).unwrap();
        symlink(&outside, &path).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
        // Recursion must use the already-open directory, not its replaced path.
        let mut ext = Ext::new(1).unwrap();
        ext.populate_dir(ROOT_INO, &path, dir).unwrap();
        let ino = find_entry(&ext, ROOT_INO, b"file");
        let inode = ext.inodes[(ino - 1) as usize];
        let off = inode.zone[0] as usize * BLOCK_SIZE;
        assert_eq!(&ext.image[off..off + inode.size as usize], b"original");
    }

    #[test]
    fn image_size_limits_are_checked_before_allocation() {
        assert_eq!(checked_image_len(1).unwrap(), 1024 * 1024);
        assert_eq!(checked_image_len(MAX_IMAGE_MB).unwrap(), 512 * 1024 * 1024);
        for size in [0, MAX_IMAGE_MB + 1, 2048, 4096, 100_000, u32::MAX] {
            assert!(Ext::new(size).is_err(), "accepted {size} MiB");
        }
    }

    #[test]
    fn allocation_capacity_errors_are_reported_instead_of_panicking() {
        let error = try_filled_vec(usize::MAX, 0u8).unwrap_err();
        assert!(error.to_string().contains("not enough memory"));
    }

    #[test]
    fn image_allocation_is_zero_initialized_and_checks_capacity() {
        assert!(try_zeroed_image(0).unwrap().is_empty());
        let image = try_zeroed_image(BLOCK_SIZE).unwrap();
        assert_eq!(image.len(), BLOCK_SIZE);
        assert!(image.iter().all(|&byte| byte == 0));
        assert!(try_zeroed_image(usize::MAX).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn explicitly_selected_source_root_can_be_a_symlink() {
        use std::os::unix::fs::symlink;
        let src = TempDir::new().unwrap();
        let real = src.path().join("real");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("file"), b"data").unwrap();
        let link = src.path().join("link");
        symlink(&real, &link).unwrap();
        let mut ext = Ext::new(1).unwrap();
        ext.populate_root(&link).unwrap();
        let ino = find_entry(&ext, ROOT_INO, b"file");
        assert_eq!(ext.inodes[(ino - 1) as usize].size, 4);
    }

    // (No name-too-long smoke test here: NAME_LEN equals the typical host
    // FS limit, so File::create would fail before populate_root sees the
    // entry. The check is exercised in the xiafs port where NAME_LEN is
    // shorter than the host limit.)
}
