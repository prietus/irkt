# AUR packages

Two packages, each lives in its own AUR repository:

- **`irkt-git`** — builds from the latest `master` with `cargo` (always current).
- **`irkt-bin`** — installs the prebuilt release binary (x86_64 / aarch64) from
  the GitHub Release assets produced by `.github/workflows/release.yml`.

Both `provide` `irkt` and conflict with each other, so only one can be installed.

## Publishing

For each package (replace `<pkg>` with `irkt-git` or `irkt-bin`):

```sh
git clone ssh://aur@aur.archlinux.org/<pkg>.git
cp packaging/aur/<pkg>/PKGBUILD <pkg>/
cd <pkg>

# Test locally first
makepkg -si        # build + install
namcap PKGBUILD    # optional lint

# Regenerate .SRCINFO from the PKGBUILD (required by the AUR)
makepkg --printsrcinfo > .SRCINFO

git add PKGBUILD .SRCINFO
git commit -m "Initial import"
git push
```

## irkt-bin checksums

`irkt-bin` ships `SKIP` placeholders. **After** the `v0.1.0` release is
published (so the `.tar.gz` assets exist), fill in real checksums:

```sh
cd irkt-bin
updpkgsums          # from the pacman-contrib package
makepkg --printsrcinfo > .SRCINFO
```

Bump `pkgver` (and re-run `updpkgsums`) on every new release.
