# mkfs-ext

Create original ext (pre-ext2) filesystem images in Rust.

The original "extended filesystem" was the first filesystem written for
Linux (Rémy Card, 1992), superseding the Minix filesystem and itself
superseded by ext2 in 1993. Modern kernels can still mount it via the
[modern-ext][modext] out-of-tree module.

[modext]: https://github.com/jeremiahsnapp/modern-ext

## Install

From crates.io:

```bash
cargo install mkfs-ext
```

This installs a binary called `mkfs-ext`. Pre-built binaries for Linux
(x86_64 / aarch64), macOS (universal), and Windows are also attached to
each [GitHub release][releases].

[releases]: https://github.com/bitplane/mkfs-ext/releases

## Usage

```bash
# Create an empty 4 MB image.
mkfs-ext out.ext 4

# Populate from a directory tree (recursive — subdirectories are preserved).
mkfs-ext -d ./payload out.ext 4

# Mount on Linux (requires the modern-ext kernel module).
sudo mount -o loop,ro -t ext out.ext /mnt
```

## Limits

- Filenames up to 255 bytes.
- 9 direct + 1 single-indirect zone slots → max file ≈ 265 KB.
- Directories are direct-only (no indirect for directory blocks).
- 32-bit block numbers.

## License

WTFPL with one extra clause: don't blame me. See [LICENSE](LICENSE).
