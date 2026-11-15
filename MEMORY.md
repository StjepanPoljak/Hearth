# Memory: what k3s takes in Hearth, and how the host gets it back

Measured on 2026-11-14 on the development laptop (i5-7200U, 4 threads,
7.6 GB RAM, EPT on), with k3s v1.37.2+k3s1 on one node: 2 vcpus, a
1536M guest, the kernel from `make-guest-kernel.sh` and the rootfs from
`make-k3s-rootfs.sh`.

## Short version

- k3s itself needs about **550–650 MB** on a node running the control
  plane: one `k3s server` process of about 650 MB resident (overlapping
  with the others by the shared pages of its binary), containerd at
  about 170 MB, and about 12 MB per pod for its containerd shim.
- The rest of the guest's RAM fills with **page cache**: about 670 MB
  of it after k3s imported its images.
- **Hearth never got any of that back.** Its resident size grew to the
  whole guest (1.59 GB for a 1.5 GB guest), and stayed there even after
  the guest freed 400 MB.
- `--balloon` gives the guest a virtio-balloon with **free page
  reporting**: the guest reports free RAM, and Hearth drops it with
  `madvise(MADV_DONTNEED)`. It works: 130 MB came back within 2 seconds
  of the guest dropping its caches, and 217 MB once the guest reported
  smaller blocks (`page_reporting.page_reporting_order=4`).

## Where the guest's memory goes

Taken once k3s had settled, about 7 minutes after boot. RSS overlaps
between processes (shared libraries, the k3s binary), so the rows add up
to more than the total.

| | stock k3s | traefik and ServiceLB off |
|---|---|---|
| Anonymous memory, whole guest (`AnonPages`) | 656 MB | 627 MB¹ |
| `k3s server` (API server, controllers, scheduler, kubelet, kube-proxy, flannel, SQLite) | 687 MB | 651 MB |
| containerd | 174 MB | 166 MB |
| containerd shims, one per pod | 7, 87 MB in all | 6, 72 MB in all |
| traefik | 99 MB | — |
| metrics-server | 63 MB | 41 MB |
| CoreDNS | 54 MB | 28 MB |
| local-path provisioner | 35 MB | 23 MB |
| Page cache (`Cached`) | 661 MB | 671 MB |
| Kernel slab, page tables, stacks | about 60 MB | about 60 MB |
| **Hearth's RSS on the host** | **1.59 GB** | **1.59 GB** |

¹ Including 63 MB for the `helm uninstall traefik` job, still running
when measured; about 560 MB without it.

Some figures in it:

- **The `k3s server` process** is most of the total. It is one Go binary
  running the Kubernetes control plane and node agent together. Each
  controller keeps an in-memory copy of every object it watches, the API
  server keeps watch caches on top of that, and Go's garbage collector
  lets the heap grow to about twice its live data before it collects.
  None of that depends on how many workloads there are; even an empty
  cluster sits at this size.
- **The add-ons** are small apart from traefik (an ingress controller
  that nothing here uses; see [SERVICELB.md](SERVICELB.md)).
- **Page cache** is the largest part that is not k3s's: what the guest
  kernel read from disk, mostly the 200 MB of compressed air-gap images
  and their unpacked layers. Linux keeps it while nothing else wants the
  memory.

### What a node needs

| node | measured | to give it |
|---|---|---|
| control plane and workloads (as here) | ~560 MB anonymous, trimmed | **1G** |
| agent only (kubelet, containerd, flannel, kube-proxy) | not measured | 512–768M, estimated |

A guest smaller than its working set of page cache isn't broken, it
reads from disk more often. Too small for its anonymous memory and it
has no swap to fall back on, so the OOM killer picks a process; at 1G
the control-plane node has about 400 MB of headroom.

## Why the host never got memory back

Hearth maps guest RAM as private anonymous memory (`mmap` with
`MAP_PRIVATE | MAP_ANONYMOUS`, in `VM::add_mem_region`) and registers it
with KVM as the guest's memory slot. The host commits a page the first
time the guest touches it, so a fresh 2G guest costs almost nothing.

After that, the host can't tell whether the guest still uses a page.
Once the guest frees a page, it is free in the guest's allocator only;
on the host it is still a dirty anonymous page that belongs to Hearth.
So Hearth's RSS only grows, up to the size of the guest. Page cache
makes this worse: Linux fills free memory with it, so every guest ends
up touching all its RAM sooner or later.

Measured before the balloon, with stock k3s settled:

```
                                  guest free   Hearth RSS on the host
before drop_caches                   121 MB          1,590,000 kB
after  echo 3 > drop_caches          514 MB          1,590,000 kB
```

The guest freed 393 MB; the host got back nothing.

## The fix: virtio-balloon with free page reporting

A virtio balloon is a device through which the guest hands
guest-physical pages back to the host. It has two modes:

1. **Inflate and deflate**, the classic balloon: the host writes a page
   count into the device's config space, and the guest driver allocates
   that many pages and reports them on the inflate queue; the host
   drops them. Deflating gives them back. This takes memory from a
   guest on the host's command, and needs a control interface to send
   the command.
2. **Free page reporting** (`VIRTIO_BALLOON_F_REPORTING`, virtio 1.2,
   section 5.5.6.7; Linux's `mm/page_reporting.c`): the guest reports
   blocks its page allocator has free on a reporting queue, by itself,
   about 2 seconds after they are freed. The host drops them and returns
   the buffers. No control interface or policy is needed.

Hearth implements the second (`src/virtio/balloon.rs`):

- The device offers only `VIRTIO_BALLOON_F_REPORTING`, so its queues are
  inflate (0), deflate (1) and reporting (2); `VRING_QUEUES_MAX` went
  from 2 to 3 for it.
- A report is a chain of device-writable buffers, one per free block,
  which Hearth's queue code already maps to host addresses. For each
  block that is whole pages, Hearth calls `madvise(MADV_DONTNEED)` on
  it: the host frees the memory, and when the guest touches the block
  again, KVM faults in a zeroed page. Reports are acknowledged with no
  data written, which is all the driver waits for.
- The config space's `num_pages`, the size the host wants the balloon to
  be, stays 0, so the driver never inflates. Pages that do come on the
  inflate and deflate queues are acknowledged and left alone.
- Linux negotiates reporting only when it can live with reported pages
  coming back zeroed: with page poisoning or `init_on_free`, it wants
  `VIRTIO_BALLOON_F_PAGE_POISON` too, which Hearth doesn't offer, and
  leaves reporting off.

How it is wired up:

- `--balloon` (or `balloon = true` in a config file), x86 only for now.
- A virtio-mmio slot at 0xd0020000 on GSI 10, described in the DSDT as
  `LNRO0005` like the others, only when `--balloon` is given (IRQ 7,
  next in line, is the 8259's spurious one).
- `CONFIG_VIRTIO_BALLOON=y` in `guest-kernel.config`, which selects
  `CONFIG_PAGE_REPORTING`.

### Measurements

Same guest, `--balloon`, trimmed k3s settled. Hearth's RSS while k3s
started fell from 1552M to 1532M without any help: reporting already
returned blocks k3s freed. Then:

```
                                   guest free   Hearth RSS on the host
before drop_caches                     82 MB          1530M
after drop_caches, +2 s               490 MB          1400M
                   +5 s, +10 s, +20 s                 1400M
```

130 MB back, but the guest had freed 408 MB.

### Why only a third came back: the reporting order

Linux reports only free blocks of at least `page_reporting_order`, by
default `pageblock_order`: 9 on x86, so 2 MB, physically contiguous.
Page cache is freed page by page, wherever the pages happen to be, and
the free memory it leaves is fragmented. `/proc/buddyinfo` after
dropping the caches (counts of free blocks per order, 4K to 4M):

```
              4K   8K  16K  32K  64K 128K 256K 512K   1M   2M   4M
zone DMA32  4278 3090 1886 1238  654  377  180   93   32   30   22
```

Of the 487 MB free, only about 150 MB was in 2M or 4M blocks.

The order is a module parameter, writable at run time or set on the
kernel command line. Refilling the page cache by reading the image
files, then dropping it again, at each order:

| `page_reporting_order` | smallest block reported | RSS before → after | returned |
|---|---|---|---|
| 9 (default) | 2 MB | 1400M → 1400M | 0 MB² |
| 4 | 64 KB | 1414M → 1197M | **217 MB** |
| 0 | 4 KB | 1400M → 1175M | 225 MB |

² The first drop at order 9 returned 130 MB; this second one returned
nothing, as the 2M blocks freed then had gone back to the cache.

Order 4 gets nearly everything order 0 does, with blocks 16 times
larger, so far fewer reports and `madvise` calls. Its cost when the
guest is busy (reports, and faults when it reuses reported blocks) has
not been measured. To use it:

```sh
hearth ... --balloon --append page_reporting.page_reporting_order=4
```

Hearth doesn't set it by itself; it could, as it sets `reboot=k`, once
its cost has been measured.

### What it doesn't do

- **It returns free memory, not page cache.** The guest gives back what
  its allocator has free, and Linux keeps page cache until something
  needs the memory. A guest that fills its RAM with cache and keeps it
  holds that RAM. Something has to make it let go: memory pressure,
  `echo 1 > /proc/sys/vm/drop_caches`, or cgroup v2's `memory.reclaim`.
- **It can't take memory away.** That is the inflate mode, which needs
  a way to tell Hearth the target size (a control socket, say).
- **Only on x86.** The device is architecture-independent, but aarch64
  and riscv64 describe their devices in device trees, and they would
  need a node for it.
- **No statistics queue** (`VIRTIO_BALLOON_F_STATS_VQ`), which would
  let the host see the guest's free and cached memory without logging
  in.

## How it was tested

- Unit tests (`cargo test`, in `src/virtio/balloon.rs`): the device
  offers reporting and three queues; a reported block of private
  anonymous pages comes back zeroed and the pages after it keep their
  data; a block that doesn't start on a page, and pages on the inflate
  queue, are left alone.
- In the guest: `virtio2` binds to the `LNRO0005:02` ACPI device, and
  `/sys/module/page_reporting/parameters/page_reporting_order` reads 9.
- The measurements above, with Hearth's RSS from `/proc/<pid>/status`.
- `guests/k3s/run.sh` runs Hearth with `--balloon` (2 vcpus, 2G) and
  checks that dropping the guest's page cache gives at least 100 MB
  back. On two runs from a fresh image, at the default order: 2028M →
  1614M and 2063M → 1669M, so about 400 MB each time: more than the
  130 MB in the 1.5G guest above. Why it differs (how fragmented each
  guest's free memory was) was not measured.

## What would help next

- Measure the CPU cost of a lower reporting order on a busy guest, then
  consider setting it by default.
- A control socket, so that the balloon can be inflated: the host could
  then take memory from an idle VM.
- The statistics queue, so that the host can see how much of a guest's
  memory is cache.
- Device tree nodes for the balloon on aarch64 and riscv64.
