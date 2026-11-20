use anyhow::{ Context, Result };
use std::io::{ self };
use std::path::Path;
use crate::{ VirtioDevice, DescChain, DiskConfig, GuestError, write_le32, new_from };
use crate::virtio_bindings::*;
use std::fs::{ OpenOptions, File };
use std::os::unix::fs::FileExt;
use std::os::fd::AsRawFd;

const SECTOR_SIZE: u64 = 512;

pub struct VirtioBlk {
    disk: File,
    /* With overlay, where the guest's writes go instead of the disk. */
    overlay: Option<Overlay>,
    /* in sectors */
    capacity: u64,
    /* Data segments per request, offered with VIRTIO_BLK_F_SEG_MAX. */
    seg_max: Option<u32>,
    /* Entries in the request queue, offered as QueueNumMax. */
    queue_size: u32
}

impl virtio_blk_outhdr {
    #[allow(dead_code)]
    fn new_from(bytes: &[u8], o: usize) -> Self {
        new_from(bytes, o).unwrap()
    }
}

/* Locks the disk image for as long as the VM runs: exclusively when the
 * VM writes to it, as two VMs writing one image would each corrupt the
 * filesystem the other has cached; shared with overlay, so that any
 * number of VMs with overlays can read it, but none while one writes it.
 * flock belongs to the open file, so it goes when Hearth exits, however
 * it exits. It only stops other users that take the lock too, as Hearth
 * does (QEMU locks with fcntl, which doesn't see it). */
fn lock(file: &File, path: &str, shared: bool) -> Result<()> {
    let op = if shared { libc::LOCK_SH } else { libc::LOCK_EX };
    if unsafe { libc::flock(file.as_raw_fd(), op | libc::LOCK_NB) } == 0 {
        return Ok(());
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::WouldBlock {
        if shared {
            anyhow::bail!("{} is in use by a VM that writes to it", path);
        }
        anyhow::bail!("{} is in use by another VM; use overlay, or give each VM its own copy", path);
    }
    Err(err).with_context(|| format!("Failed to lock {}", path))
}

/* The overlay, a copy-on-write layer: the sectors the guest wrote, at their
 * own offsets in a sparse file the size of the disk, and a bit for each
 * sector that says whether it is there. The file sits next to the image,
 * not in /tmp, which is often in RAM, and is unlinked as soon as it is
 * made, so it goes with Hearth. */
struct Overlay {
    file: File,
    written: Vec<u64>
}

impl Overlay {
    fn new(image: &str, sectors: u64) -> Result<Self> {
        let image = Path::new(image);
        let dir = image.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let name = image.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        let path = dir.join(format!(".{}.overlay-{}", name, std::process::id()));
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(&path)
            .with_context(|| format!("Failed to create the overlay {}", path.display()))?;
        std::fs::remove_file(&path)?;
        file.set_len(sectors * SECTOR_SIZE)?;
        Ok(Self { file, written: vec![0; sectors.div_ceil(64) as usize] })
    }

    fn is_written(&self, sector: u64) -> bool {
        self.written[(sector / 64) as usize] & (1 << (sector % 64)) != 0
    }

    /* How many sectors from `sector`, at most `max`, are like it: all
     * written or all not. */
    fn run(&self, sector: u64, max: u64) -> (bool, u64) {
        let written = self.is_written(sector);
        let len = (1..max).find(|&i| self.is_written(sector + i) != written).unwrap_or(max);
        (written, len)
    }

    fn write(&mut self, data: &[u8], offset: u64) -> io::Result<()> {
        self.file.write_all_at(data, offset)?;
        let first = offset / SECTOR_SIZE;
        for sector in first..first + data.len() as u64 / SECTOR_SIZE {
            self.written[(sector / 64) as usize] |= 1 << (sector % 64);
        }
        Ok(())
    }
}

impl VirtioBlk {
    pub fn new(config: &DiskConfig) -> Result<Self> {
        let DiskConfig { path, queue_size, seg_max, overlay: with_overlay } = config;
        let file = OpenOptions::new()
            .read(true)
            .write(!with_overlay)
            .open(path)
            .with_context(|| format!("Failed to open {}", path))?;
        lock(&file, path, *with_overlay)?;
        let capacity = file.metadata()?.len() / SECTOR_SIZE;
        let overlay = if *with_overlay { Some(Overlay::new(path, capacity)?) } else { None };

        Ok(Self { disk: file, overlay, capacity, seg_max: *seg_max, queue_size: *queue_size })
    }

    /* Checks that `len` bytes from `sector` lie within the disk, returns the byte offset. */
    fn disk_offset(&self, sector: u64, len: usize) -> io::Result<u64> {
        let end = sector.checked_mul(SECTOR_SIZE)
            .and_then(|offset| offset.checked_add(len as u64).map(|end| (offset, end)));
        match end {
            Some((offset, end)) if end <= self.capacity * SECTOR_SIZE => Ok(offset),
            _ => Err(io::Error::other(format!("Virtio-blk access past end of disk (sector {:#x}, {} bytes).", sector, len)))
        }
    }

    /* Reads `buf` from `offset`: from the disk, or with overlay, in runs
     * from the overlay where the guest wrote and from the disk elsewhere. */
    fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<()> {
        let Some(overlay) = &self.overlay else {
            return self.disk.read_exact_at(buf, offset);
        };
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let left = (buf.len() - done) as u64;
            let into = at % SECTOR_SIZE;
            let (written, sectors) = overlay.run(at / SECTOR_SIZE, (into + left).div_ceil(SECTOR_SIZE));
            let len = (sectors * SECTOR_SIZE - into).min(left) as usize;
            let file = if written { &overlay.file } else { &self.disk };
            file.read_exact_at(&mut buf[done..done + len], at)?;
            done += len;
        }
        Ok(())
    }

    fn read(&self, sector: u64, data: &mut [&mut [u8]]) -> io::Result<u32> {
        let total = data.iter().map(|buf| buf.len()).sum();
        let mut offset = self.disk_offset(sector, total)?;
        for buf in data.iter_mut() {
            self.read_at(buf, offset)?;
            offset += buf.len() as u64;
        }
        Ok(total as u32)
    }

    /* With overlay, only whole sectors: the overlay keeps no part of one. */
    fn write(&mut self, sector: u64, data: &[u8]) -> io::Result<()> {
        let offset = self.disk_offset(sector, data.len())?;
        match &mut self.overlay {
            None => self.disk.write_all_at(data, offset),
            Some(_) if data.len() as u64 % SECTOR_SIZE != 0 =>
                Err(io::Error::other(format!("Virtio-blk write of {} bytes, not whole sectors, to an overlay.", data.len()))),
            Some(overlay) => overlay.write(data, offset)
        }
    }

    /* An overlay is dropped at exit, so there is nothing to flush. */
    fn flush(&self) -> io::Result<()> {
        if self.overlay.is_some() { Ok(()) } else { self.disk.sync_data() }
    }
}

impl VirtioDevice for VirtioBlk {
    fn device_id(&self) -> u32 {
        VIRTIO_ID_BLOCK
    }

    fn name(&self) -> &'static str {
        "virtio-blk"
    }

    fn features(&self, sel: u32) -> u32 {
        match sel {
            0 => (1 << VIRTIO_BLK_F_FLUSH)
                | if self.seg_max.is_some() { 1 << VIRTIO_BLK_F_SEG_MAX } else { 0 },
            _ => 0x0
        }
    }

    fn queue_num_max(&self, queue: u32) -> u32 {
        if queue == 0 { self.queue_size } else { 0 }
    }

    fn read_config(&mut self, offset: u32, data: &mut [u8]) -> Result<()> {
        match offset {
            /* capacity is measured in 512 bytes */
            0x0 => write_le32(data, 0, self.capacity as u32), /* capacity low */
            0x4 => write_le32(data, 0, (self.capacity >> 32) as u32), /* capacity high */
            0xc => write_le32(data, 0, self.seg_max.unwrap_or(0)), /* seg_max */
            _   => println!("Unknown config at {:#x}", offset)
        };
        Ok(())
    }

    /* Disk errors go back to the guest as VIRTIO_BLK_S_IOERR; a request
     * with no header or status byte has nowhere to put one. */
    fn process(&mut self, _queue: u32, chain: &mut DescChain) -> Result<u32, GuestError> {
        let outhdr = new_from::<virtio_blk_outhdr>(&chain.read_buf, 0x0)
            .ok_or(GuestError("request without header".into()))?;
        let (status, data) = chain.write_buf.split_last_mut()
            .ok_or(GuestError("request without status byte".into()))?;
        let out_data = &chain.read_buf[std::mem::size_of::<virtio_blk_outhdr>()..];

        let (res, written) = match outhdr.type_ {
            VIRTIO_BLK_T_IN => match self.read(outhdr.sector, data) {
                Ok(len) => (Ok(()), len),
                Err(e) => (Err(e), 0)
            },
            VIRTIO_BLK_T_OUT => (self.write(outhdr.sector, out_data), 0),
            VIRTIO_BLK_T_FLUSH => (self.flush(), 0),
            _ => {
                status[0] = VIRTIO_BLK_S_UNSUPP as u8;
                return Ok(1);
            }
        };

        status[0] = match res {
            Ok(()) => VIRTIO_BLK_S_OK,
            Err(e) => { println!("Virtio-blk request failed: {}\r", e); VIRTIO_BLK_S_IOERR }
        } as u8;
        /* The used length counts every byte written to the guest, status included. */
        Ok(written + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(path: &str, overlay: bool) -> DiskConfig {
        DiskConfig { path: path.to_string(), queue_size: 128, seg_max: None, overlay }
    }

    /* An 8-sector image, sector i filled with byte i, in its own directory
     * (removed by the caller), so that overlays are made there. */
    fn image(name: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("hearth-blk-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.img");
        let data: Vec<u8> = (0..8u8).flat_map(|i| [i; SECTOR_SIZE as usize]).collect();
        std::fs::write(&path, data).unwrap();
        let s = path.to_str().unwrap().to_string();
        (dir, s)
    }

    fn read(blk: &VirtioBlk, sector: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        blk.read(sector, &mut [&mut buf[..]]).unwrap();
        buf
    }

    #[test]
    fn a_disk_open_in_one_vm_cannot_be_opened_in_another() {
        let (dir, path) = image("lock");
        let first = VirtioBlk::new(&config(&path, false)).unwrap();
        let err = VirtioBlk::new(&config(&path, false)).err().expect("the second open must fail");
        assert!(err.to_string().contains("in use by another VM"), "{}", err);
        let err = VirtioBlk::new(&config(&path, true)).err().expect("an overlay must fail too");
        assert!(err.to_string().contains("in use by a VM that writes"), "{}", err);

        /* Closing the first frees the image. */
        drop(first);
        assert!(VirtioBlk::new(&config(&path, false)).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn overlays_share_the_image_and_keep_their_writes() {
        let (dir, path) = image("overlay");
        let before = std::fs::read(&path).unwrap();
        let mut a = VirtioBlk::new(&config(&path, true)).unwrap();
        let mut b = VirtioBlk::new(&config(&path, true)).unwrap();
        let err = VirtioBlk::new(&config(&path, false)).err().expect("no writer beside overlays");
        assert!(err.to_string().contains("in use by another VM"), "{}", err);

        a.write(2, &[0xaa; 2 * SECTOR_SIZE as usize]).unwrap();
        b.write(3, &[0xbb; SECTOR_SIZE as usize]).unwrap();

        /* A read across written and unwritten sectors, in two buffers that
         * split a sector, comes from both places. */
        let mut x = vec![0u8; 700];
        let mut y = vec![0u8; 5 * SECTOR_SIZE as usize - 700];
        a.read(1, &mut [&mut x[..], &mut y[..]]).unwrap();
        let got: Vec<u8> = x.into_iter().chain(y).collect();
        let want: Vec<u8> = [1u8, 0xaa, 0xaa, 4, 5].iter().flat_map(|&i| [i; SECTOR_SIZE as usize]).collect();
        assert_eq!(got, want);
        assert_eq!(read(&b, 2, 1024), [[2u8; 512], [0xbb; 512]].concat());

        assert!(a.write(0, &[0; 100]).is_err(), "part of a sector");
        assert_eq!(std::fs::read(&path).unwrap(), before, "the image itself is untouched");

        /* Each overlay is unlinked: nothing is left beside the image. */
        drop(a);
        drop(b);
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["disk.img"]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
