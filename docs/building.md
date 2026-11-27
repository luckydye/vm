# Building from source

On macOS, requires macOS 26 or later, Xcode, Rust, Docker and the Zig of `mise.toml` (`mise install`), which builds Ghostty's terminal for `vm ui`; the first build fetches Ghostty's source.

```sh
brew install cmake meson ninja pkg-config python
brew tap nirs/vmnet-helper && brew trust nirs/vmnet-helper && brew install vmnet-helper
make build                         # dist/vm: bin/vm and share/vm
make install                       # into ~/.local, or INSTALL_PREFIX=...
make pkg                           # dist/vm.pkg
```

On Linux, requires Rust, Docker and the Zig of `mise.toml`; the guest image is built for the host's architecture, or `GUEST_ARCH=arm64|amd64`.

```sh
sudo apt install libvirglrenderer-dev libx264-dev libopus-dev libpulse-dev libsqlite3-dev libavcodec-dev \
  libfontconfig-dev libwayland-dev libxkbcommon-x11-dev pkg-config
make build                         # dist/vm and dist/vm-linux-ARCH.tar.gz
make install                       # into ~/.local, or INSTALL_PREFIX=...
```
