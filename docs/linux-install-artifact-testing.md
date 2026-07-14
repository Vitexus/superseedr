# Linux install artifact testing

Superseedr Linux release artifacts can be smoke-tested locally with Docker before they are promoted into a release. The test verifies the tarball binary in a clean Linux container before installing the Debian package, verifies the installed binary starts, and purges the package again.

## Script

```sh
scripts/test_linux_install_artifacts.sh --artifact-dir DIR --platform linux/amd64
scripts/test_linux_install_artifacts.sh --artifact-dir DIR --platform linux/arm64
```

`DIR` must contain exactly one `.deb` and exactly one `.tar.gz` for a single release variant, plus an optional `SHA256SUMS` file.

The script checks:

- `SHA256SUMS`, when present
- `.deb` metadata and contents
- tarball extraction, dynamic library resolution, and tarball binary `--help` before package installation
- `apt-get install` of the local `.deb`
- `dpkg -s superseedr` and `dpkg -L superseedr`
- `/usr/bin/superseedr` architecture and dynamic library resolution
- `superseedr --help`
- `apt-get purge` removes the package and installed binary

## Examples

Test an amd64 release artifact directory with Ubuntu 24.04:

```sh
scripts/test_linux_install_artifacts.sh \
  --artifact-dir /tmp/superseedr-linux-amd64-normal \
  --platform linux/amd64 \
  --image ubuntu:24.04
```

Test an arm64 release artifact directory with Ubuntu 24.04:

```sh
scripts/test_linux_install_artifacts.sh \
  --artifact-dir /tmp/superseedr-linux-arm64-normal \
  --platform linux/arm64 \
  --image ubuntu:24.04
```

Test against Debian instead of Ubuntu:

```sh
scripts/test_linux_install_artifacts.sh \
  --artifact-dir /tmp/superseedr-linux-amd64-normal \
  --platform linux/amd64 \
  --image debian:bookworm
```

## Artifact layout

The current release workflows upload one artifact per architecture/flavor. After downloading and unzipping a GitHub Actions artifact, each test directory should look like this:

```text
SHA256SUMS
superseedr_<version>_<arch>.deb
superseedr_<version>_linux-<arch>.tar.gz
```

Private artifacts use the same layout with `superseedr-private_...` filenames.

Tarballs should contain a top-level release directory with:

```text
superseedr
README.md
LICENSE
CHANGELOG.md
```

## Notes

- Docker must be running locally.
- On Apple Silicon, `--platform linux/arm64` runs ARM Linux userspace naturally through Docker Desktop. `--platform linux/amd64` runs under emulation.
- The script is intended as a local/manual smoke test. CI release jobs still build and upload the artifacts; this script is useful when inspecting downloaded artifacts or reproducing packaging issues locally.
