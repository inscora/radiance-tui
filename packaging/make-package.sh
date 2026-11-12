#!/usr/bin/env bash
# Build the Arch package from a clean git checkout.
#
#   ./packaging/make-package.sh          # writes packaging/*.pkg.tar.zst
#
# cargo-vendors all crates into vendor/ (+ .cargo/config.toml redirect) so the
# PKGBUILD builds fully offline with --locked --offline, then tars the sources,
# refreshes the PKGBUILD checksum, and runs makepkg.
set -euo pipefail
cd "$(dirname "$0")/.."

version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
name=radiance-tui
stage=/tmp/$name-$version
tarball=packaging/$name-$version.tar.gz

echo "==> vendoring crates"
cargo vendor vendor --versioned-dirs --quiet >/dev/null
mkdir -p .cargo
printf '[source.crates-io]\nreplace-with = "vendored-sources"\n\n[source.vendored-sources]\ndirectory = "vendor"\n' > .cargo/config.toml

echo "==> staging sources -> $stage"
rm -rf "$stage"
mkdir -p "$(dirname "$stage")"
cp -r --reflink=auto . "$stage"
rm -rf "$stage/target" "$stage/packaging/src" "$stage/packaging/pkg"

echo "==> tarball -> $tarball"
tar --transform "s,^$name-$version,$name-$version," -czf "/tmp/$name-$version.tar.gz" -C "$(dirname "$stage")" "$(basename "$stage")"
mv "/tmp/$name-$version.tar.gz" "$tarball"
sum=$(sha256sum "$tarball" | cut -d' ' -f1)
sed -i "s/^sha256sums=.*/sha256sums=('$sum')/" packaging/PKGBUILD

echo "==> makepkg"
cd packaging
makepkg -f --cleanbuild --noconfirm

echo
ls -la "$name-$version"-*.pkg.tar.zst
