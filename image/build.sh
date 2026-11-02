#!/bin/sh
# Runs inside the builder container: build.sh <x86_64|aarch64> <minimal|desktop>, repo at /src, output at /out.
set -eu
ARCH=$1
PROFILE=$2
KERNEL_VERSION=6.12.115
KERNEL_SHA256=0ec90fe4b20422b1b664b77e6d2dde22b40bd41e7eb2cad8ac8739a4749cb1bb
SNAPSHOT=20261001T000000Z
export SOURCE_DATE_EPOCH=1793491200 # 2026-11-01
export KBUILD_BUILD_TIMESTAMP="@$SOURCE_DATE_EPOCH" KBUILD_BUILD_USER=thanatos KBUILD_BUILD_HOST=thanatos
case $ARCH in
    x86_64) KARCH=x86 DEB=amd64 IMAGE=bzImage DEFCONFIG=x86_64_defconfig KPATH=arch/x86/boot/bzImage ;;
    aarch64) KARCH=arm64 DEB=arm64 IMAGE=Image DEFCONFIG=defconfig KPATH=arch/arm64/boot/Image ;;
    *) echo "unknown arch $ARCH" >&2; exit 1 ;;
esac

apt-get update -qq
apt-get install -qq -y --no-install-recommends build-essential bc bison flex libelf-dev libssl-dev \
    xz-utils curl ca-certificates mmdebstrap e2fsprogs cpio perl musl-tools >/dev/null

if [ ! -f "/out/$IMAGE" ]; then
    curl -fsSL "https://cdn.kernel.org/pub/linux/kernel/v6.x/linux-$KERNEL_VERSION.tar.xz" -o /tmp/linux.tar.xz
    echo "$KERNEL_SHA256  /tmp/linux.tar.xz" | sha256sum -c -
    tar -xf /tmp/linux.tar.xz -C /tmp
    cd "/tmp/linux-$KERNEL_VERSION"
    make -s ARCH=$KARCH $DEFCONFIG
    cat /src/image/kernel/common.config "/src/image/kernel/$ARCH.config" >> .config
    make -s ARCH=$KARCH olddefconfig
    for line in $(grep -hv '^$' /src/image/kernel/common.config "/src/image/kernel/$ARCH.config"); do
        # "=n" reads back as "is not set"; anything else that did not stick is a broken recipe.
        case $line in
            *=n) ! grep -q "^${line%=n}=" .config ;;
            *) grep -q "^$line$" .config ;;
        esac || { echo "kernel option did not stick: $line" >&2; exit 1; }
    done
    make -s -j"$(nproc)" ARCH=$KARCH "$IMAGE"
    cp "$KPATH" "/out/$IMAGE"
fi

# The agent: pinned toolchain from the repo, static musl so it runs on any guest userland.
curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain none
. "$HOME/.cargo/env"
cd /src
rustup target add "$ARCH-unknown-linux-musl"
CARGO_TARGET_DIR=/tmp/agent cargo build --locked --release -p thanatos-guest --target "$ARCH-unknown-linux-musl"
AGENT="/tmp/agent/$ARCH-unknown-linux-musl/release/thanatos-guest"

PACKAGES=$(cat /src/image/rootfs/packages.txt "/src/image/profiles/$PROFILE.txt" | tr '\n' ',' | sed 's/,$//')
ROOT=/tmp/root
rm -rf $ROOT
mmdebstrap --mode=root --variant=minbase --architectures=$DEB --include="$PACKAGES" \
    --aptopt='Acquire::Check-Valid-Until "false"' \
    trixie $ROOT "deb http://snapshot.debian.org/archive/debian/$SNAPSHOT trixie main"

cat > $ROOT/etc/fstab <<FSTAB
/dev/vda / ext4 defaults 0 1
FSTAB
echo thanatos > $ROOT/etc/hostname
mkdir -p $ROOT/etc/systemd/network
cat > $ROOT/etc/systemd/network/20-dhcp.network <<NET
[Match]
Name=en* eth*
[Network]
DHCP=ipv4
NET
cat > $ROOT/etc/systemd/system/thanatos-grow.service <<UNIT
[Unit]
Description=Grow the root filesystem to fill its disk
[Service]
Type=oneshot
ExecStart=/usr/sbin/resize2fs /dev/vda
[Install]
WantedBy=multi-user.target
UNIT
install -m 0755 "$AGENT" $ROOT/usr/local/bin/thanatos-guest
cat > $ROOT/etc/systemd/system/thanatos-guest.service <<UNIT
[Unit]
Description=Thanatos guest agent
[Service]
ExecStart=/usr/local/bin/thanatos-guest
Restart=always
RestartSec=1
[Install]
WantedBy=multi-user.target
UNIT
chroot $ROOT useradd -m -s /bin/bash -G sudo user
chroot $ROOT passwd -d user
chroot $ROOT passwd -d root
echo 'user ALL=(ALL) NOPASSWD:ALL' > $ROOT/etc/sudoers.d/user
chroot $ROOT systemctl enable systemd-networkd systemd-resolved thanatos-grow thanatos-guest
chroot $ROOT systemctl disable ssh 2>/dev/null || true
ln -sf /run/systemd/resolve/stub-resolv.conf $ROOT/etc/resolv.conf
rm -f $ROOT/etc/machine-id $ROOT/var/lib/dbus/machine-id
: > $ROOT/etc/machine-id
rm -rf $ROOT/var/cache/apt/* $ROOT/var/lib/apt/lists/* $ROOT/var/log/* $ROOT/tmp/*
find $ROOT -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

MIB=$(( $(du -sm $ROOT | cut -f1) + 512 ))
rm -f /out/root.ext4
E2FSPROGS_FAKE_TIME=$SOURCE_DATE_EPOCH mke2fs -q -t ext4 -d $ROOT -L root \
    -U 7a1f3b52-6c1d-4e8a-9f0b-5d3c2e1a4b6f -E hash_seed=7a1f3b52-6c1d-4e8a-9f0b-5d3c2e1a4b6f,lazy_itable_init=0 \
    /out/root.ext4 "${MIB}M"
