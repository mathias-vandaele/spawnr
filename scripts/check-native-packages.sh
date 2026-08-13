#!/bin/sh
set -eu

if [ "$#" -ne 6 ]; then
    echo 'usage: check-native-packages.sh STATIC_CLI LICENSE DEB RPM PKGBUILD SRCINFO' >&2
    exit 2
fi

static_cli=$1
license=$2
deb=$3
rpm_package=$4
pkgbuild=$5
srcinfo=$6
makepkg_conf=${MAKEPKG_CONF:-/etc/makepkg.conf}
test_root=$(mktemp -d "${TMPDIR:-/tmp}/spawnr-native-packages.XXXXXX")
trap 'rm -rf -- "$test_root"' EXIT HUP INT TERM

assert_equal() {
    expected=$1
    actual=$2
    label=$3
    if [ "$actual" != "$expected" ]; then
        printf '%s: expected %s, got %s\n' "$label" "$expected" "$actual" >&2
        exit 1
    fi
}

assert_equal spawnr "$(dpkg-deb --field "$deb" Package)" 'deb package name'
assert_equal 0.1.0-1 "$(dpkg-deb --field "$deb" Version)" 'deb version'
assert_equal amd64 "$(dpkg-deb --field "$deb" Architecture)" 'deb architecture'
if dpkg-deb --field "$deb" Depends 2>/dev/null | grep -q .; then
    echo 'deb package unexpectedly declares runtime dependencies' >&2
    exit 1
fi
if dpkg-deb --ctrl-tarfile "$deb" \
    | tar --list --file=- \
    | grep -Eq '(^|/)(preinst|postinst|prerm|postrm|config|triggers)$'; then
    echo 'deb package unexpectedly contains maintainer scripts or triggers' >&2
    exit 1
fi
mkdir "$test_root/deb"
dpkg-deb --extract "$deb" "$test_root/deb"
cmp "$static_cli" "$test_root/deb/usr/bin/spawnr"
"$test_root/deb/usr/bin/spawnr" --version | grep -Fx 'spawnr 0.1.0'
assert_equal 3 "$(find "$test_root/deb" -type f | wc -l)" 'deb regular file count'

mkdir "$test_root/rpmdb"
assert_equal spawnr "$(rpm --dbpath "$test_root/rpmdb" --query --package --queryformat '%{NAME}' "$rpm_package")" 'rpm package name'
assert_equal 0.1.0 "$(rpm --dbpath "$test_root/rpmdb" --query --package --queryformat '%{VERSION}' "$rpm_package")" 'rpm version'
assert_equal 1 "$(rpm --dbpath "$test_root/rpmdb" --query --package --queryformat '%{RELEASE}' "$rpm_package")" 'rpm release'
assert_equal x86_64 "$(rpm --dbpath "$test_root/rpmdb" --query --package --queryformat '%{ARCH}' "$rpm_package")" 'rpm architecture'
if [ -n "$(rpm --dbpath "$test_root/rpmdb" --query --package --scripts "$rpm_package")" ]; then
    echo 'rpm package unexpectedly contains transaction scripts' >&2
    exit 1
fi
if rpm --dbpath "$test_root/rpmdb" --query --package --requires "$rpm_package" | grep -q .; then
    echo 'rpm package unexpectedly declares runtime dependencies' >&2
    exit 1
fi
mkdir "$test_root/rpm"
(
    cd "$test_root/rpm"
    rpm2cpio "$rpm_package" | cpio --extract --make-directories --quiet
)
cmp "$static_cli" "$test_root/rpm/usr/bin/spawnr"
"$test_root/rpm/usr/bin/spawnr" --version | grep -Fx 'spawnr 0.1.0'
assert_equal 3 "$(find "$test_root/rpm" -type f | wc -l)" 'rpm regular file count'

bash -n "$pkgbuild"
mkdir "$test_root/aur"
cp "$pkgbuild" "$test_root/aur/PKGBUILD"
(
    cd "$test_root/aur"
    makepkg --config "$makepkg_conf" --printsrcinfo
) > "$test_root/generated.SRCINFO"
cmp "$srcinfo" "$test_root/generated.SRCINFO"
grep -Fx 'pkgbase = spawnr-bin' "$srcinfo"
grep -Fx 'pkgname = spawnr-bin' "$srcinfo"
grep -Eq '^[[:space:]]+arch = x86_64$' "$srcinfo"
grep -Eq '^[[:space:]]+options = !strip$' "$srcinfo"
grep -F 'https://github.com/spawnr-dev/spawnr/releases/download/v0.1.0/' "$srcinfo"
cli_sha=$(sha256sum "$static_cli" | cut -d ' ' -f 1)
grep -Eq "^[[:space:]]+sha256sums_x86_64 = $cli_sha$" "$srcinfo"
license_sha=$(sha256sum "$license" | cut -d ' ' -f 1)
grep -Eq "^[[:space:]]+sha256sums_x86_64 = $license_sha$" "$srcinfo"

cp "$static_cli" "$test_root/aur/spawnr-0.1.0-x86_64-linux"
cp "$license" "$test_root/aur/LICENSE"
mkdir "$test_root/arch"
(
    cd "$test_root/aur"
    SPAWNR_AUR_TEST_PKGDIR="$test_root/arch" \
        bash -eu -c 'pkgdir=$SPAWNR_AUR_TEST_PKGDIR; source ./PKGBUILD; package'
)
cmp "$static_cli" "$test_root/arch/usr/bin/spawnr"
assert_equal 2 "$(find "$test_root/arch/usr" -type f | wc -l)" 'Arch regular file count'
