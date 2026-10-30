ARG FEX_COMMIT=0df84d3844bcdb87bb7d3f5b8fb0959cd009c038

FROM debian:trixie-slim AS fex-builder

ARG FEX_COMMIT
ARG TARGETARCH
ENV DEBIAN_FRONTEND=noninteractive

# FEX runs x86-64 programs on arm64 guests; x86-64 guests get an empty install.

RUN [ "$TARGETARCH" = arm64 ] || exit 0; \
    apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        clang \
        clang-tools \
        cmake \
        git \
        libasound2-dev \
        libc6-dev-amd64-cross \
        libc6-dev-i386-cross \
        libclang-19-dev \
        libdrm-dev \
        libegl-dev \
        libgl-dev \
        libssl-dev \
        libstdc++-14-dev-amd64-cross \
        libstdc++-14-dev-i386-cross \
        libwayland-dev \
        libx11-dev \
        libxcb1-dev \
        libxrandr-dev \
        libxrender-dev \
        lld \
        llvm-19-dev \
        ninja-build \
        pkgconf && \
    rm -rf /var/lib/apt/lists/*

RUN [ "$TARGETARCH" = arm64 ] || exit 0; \
    git clone https://github.com/FEX-Emu/FEX.git /src/FEX && \
    git -C /src/FEX checkout "$FEX_COMMIT" && \
    git -C /src/FEX submodule update --init --recursive && \
    sed -i '/Opening host-side X11 display:/s/fprintf/\/\/ fprintf/' /src/FEX/ThunkLibs/include/common/X11Manager.h && \
    sed -i '/Linking address %p to host invoker/s/fprintf/\/\/ fprintf/' /src/FEX/ThunkLibs/libvulkan/Guest.cpp

RUN if [ "$TARGETARCH" != arm64 ]; then mkdir -p /opt/fex-install; exit 0; fi; \
    cmake \
        -S /src/FEX \
        -B /src/FEX/build \
        -G Ninja \
        -DCMAKE_BUILD_TYPE=Release \
        -DCMAKE_C_COMPILER=/usr/bin/clang-19 \
        -DCMAKE_CXX_COMPILER=/usr/bin/clang++-19 \
        -DCMAKE_INSTALL_PREFIX=/usr \
        -DCMAKE_INSTALL_LIBDIR=lib/aarch64-linux-gnu \
        -DBUILD_FEXCONFIG=OFF \
        -DBUILD_FEX_LINUX_TESTS=OFF \
        -DBUILD_TESTING=OFF \
        -DBUILD_THUNKS=ON \
        -DENABLE_ASSERTIONS=OFF \
        -DENABLE_CLANG_THUNKS=ON \
        -DTUNE_ARCH=generic \
        -DTUNE_CPU=generic \
        -DUSE_LINKER=lld \
        -DX86_DEV_ROOTFS=/ && \
    cmake --build /src/FEX/build --parallel && \
    DESTDIR=/opt/fex-install cmake --install /src/FEX/build && \
    test -x /opt/fex-install/usr/bin/FEXBash && \
    test -x /opt/fex-install/usr/bin/FEXRootFSFetcher && \
    test -f /opt/fex-install/usr/share/fex-emu/GuestThunks/libvulkan-guest.so && \
    test -f /opt/fex-install/usr/lib/aarch64-linux-gnu/fex-emu/HostThunks/libvulkan-host.so && \
    test -f /opt/fex-install/usr/lib/aarch64-linux-gnu/fex-emu/HostThunks_32/libvulkan-host.so

FROM debian:trixie-slim AS hyprland-builder

ENV DEBIAN_FRONTEND=noninteractive

# Hyprland from trixie-backports with varmint's patches (patches/hyprland),
# built against the archive snapshot from its upload: backports has since moved
# its -dev packages to versions Hyprland 0.55.2 does not compile against.
COPY patches/hyprland /opt/hyprland-patches

RUN printf 'Types: deb deb-src\nURIs: http://snapshot.debian.org/archive/debian/20260610T000000Z\nSuites: trixie-backports\nComponents: main\nSigned-By: /usr/share/keyrings/debian-archive-keyring.gpg\nCheck-Valid-Until: no\n' \
        > /etc/apt/sources.list.d/backports-snapshot.sources && \
    apt-get update && \
    apt-get install -y --no-install-recommends dpkg-dev && \
    apt-get build-dep -y -t trixie-backports hyprland && \
    mkdir /src && cd /src && apt-get source -t trixie-backports hyprland && \
    cd hyprland-*/ && \
    for patch in /opt/hyprland-patches/*.patch; do cp "$patch" debian/patches/ && basename "$patch" >> debian/patches/series; done && \
    DEB_BUILD_OPTIONS="nocheck parallel=$(nproc)" dpkg-buildpackage -b -uc -us && \
    install -Dm0644 -t /opt/hyprland-debs /src/hyprland_*.deb

FROM debian:trixie-slim

ARG TARGETARCH
ENV DEBIAN_FRONTEND=noninteractive \
    LIBGUESTFS_BACKEND=direct \
    LIBGUESTFS_BACKEND_SETTINGS=force_tcg

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        libguestfs-tools \
        linux-image-$TARGETARCH \
        ipxe-qemu \
        qemu-utils \
        zstd && \
    rm -rf /var/lib/apt/lists/*

COPY --from=fex-builder /opt/fex-install /opt/varmint/fex-install
COPY --from=hyprland-builder /opt/hyprland-debs /opt/varmint/hyprland-debs
COPY guest /opt/varmint/source

RUN install -Dm0755 /opt/varmint/source/build-image-container.sh /usr/local/bin/build-varmint-guest && \
    install -Dm0755 /opt/varmint/source/varmint-gpu-check /opt/varmint/rootfs/usr/local/bin/varmint-gpu-check && \
    install -Dm0755 /opt/varmint/source/varmint-agent /opt/varmint/rootfs/usr/local/libexec/varmint-agent && \
    install -Dm0755 /opt/varmint/source/varmint-clipboard-agent /opt/varmint/rootfs/usr/local/libexec/varmint-clipboard-agent && \
    install -Dm0755 /opt/varmint/source/varmint-display-fit /opt/varmint/rootfs/usr/local/libexec/varmint-display-fit && \
    install -Dm0755 /opt/varmint/source/varmint-grow-root /opt/varmint/rootfs/usr/local/libexec/varmint-grow-root && \
    install -Dm0755 /opt/varmint/source/varmint-provision /opt/varmint/rootfs/usr/local/libexec/varmint-provision && \
    if [ "$TARGETARCH" = arm64 ]; then \
        install -Dm0755 /opt/varmint/source/varmint-fex /opt/varmint/rootfs/usr/local/bin/FEX && \
        ln -s FEX /opt/varmint/rootfs/usr/local/bin/FEXBash && \
        ln -s FEX /opt/varmint/rootfs/usr/local/bin/FEXRootFSFetcher; \
    fi && \
    install -Dm0644 /opt/varmint/source/mise.toml /opt/varmint/rootfs/etc/mise/config.toml && \
    install -Dm0755 /opt/varmint/source/varmint-initramfs-fsck-hook /opt/varmint/rootfs/etc/initramfs-tools/hooks/varmint-fsck && \
    install -Dm0644 /opt/varmint/source/varmint-agent.service /opt/varmint/rootfs/etc/systemd/system/varmint-agent.service && \
    install -Dm0644 /opt/varmint/source/varmint-grow-root.service /opt/varmint/rootfs/etc/systemd/system/varmint-grow-root.service && \
    install -Dm0644 /opt/varmint/source/hyprland.conf /opt/varmint/rootfs/etc/skel/.config/hypr/hyprland.conf && \
    rm -rf /opt/varmint/source

ENTRYPOINT ["/usr/local/bin/build-varmint-guest"]
