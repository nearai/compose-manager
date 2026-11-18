# Offline APT bundle

The root Dockerfile uses the immutable `image` reference in `bundle.lock.json`.
This locally assembled OCI image contains `/apt-bundle`: original signed Debian,
Debian Security and Docker repository metadata, the 36 exact Debian downloads in
`debian-packages.txt` from snapshot `20251020T000000Z`, and the three exact amd64
Docker packages in `docker-packages.txt`. These are download closures, not an
inventory of inherited base packages. Docker CLI, Compose and buildx keep their
original explicit versions in the Dockerfile.

`COPY --from=apt-bundle` exposes data in a scratch stage; read-only RUN mounts
keep repository contents out of application layers. APT runs with `--network=none`
using the independently reviewed, source-controlled `prepare-apt.sh` included
here. Its SHA256 is recorded in the lock. The helper is never loaded from the
bundle. It creates isolated file-only APT configuration and uses the pinned base
image's `/usr/share/keyrings/debian-archive-keyring.gpg` plus the
source-controlled `docker.asc`, whose primary fingerprint is
`9DC858229FC7DD38854AE2D88D81803C0EBFCD88`. The Docker key was copied from the
independently verified key source. No trust key comes from the bundle. APT
verifies repository signatures and package checksums. Base automatic-package state
is copied into the isolated state, then written back after purge/autoremove and
before configuration/state cleanup in the RUN; CA revocation assertions,
curl/gnupg purge, autoremove and reproducible cleanup remain mandatory.

For an update, regenerate the exact closures from the selected signed repositories
and unchanged base/versions, assemble the OCI data image using the separately
reviewed shared tooling, verify its manifest digest and inventory, and publish it.
Update the Dockerfile, manifests and lock together. CI/release review must compare
the source manifests against the artifact, verify signed indexes and package
checksums using independent keys, anonymously pull the public digest, and run full
application reproducibility and existing package-pin comparison checks. There is
no automatic publisher or secret-handling workflow in this repository.

Helper integration is complete; publication remains pending. Do not deploy this
candidate before public anonymous bundle pull and full reproducibility
verification pass. This changes build inputs and requires a new Brave-approved
application digest; it does not preserve or approve any previous application
digest. Existing `build-image.sh` options remain unchanged.
