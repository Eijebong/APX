#!/bin/bash

set -e

apt update && apt install -y curl
cat > /etc/apt/sources.list.d/debian-backports.sources <<EOF
Types: deb deb-src
URIs: http://deb.debian.org/debian
Suites: bookworm-backports
Components: main
Enabled: yes
Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg
EOF
apt update && apt install -y libpq-dev python3 git libssl-dev pkg-config liblzma-dev zlib1g-dev
apt autoremove -y
rm -rf /var/lib/apt/lists/*
rustup component add clippy rustfmt

# Add worker user
mkdir -p /builds
useradd -d /builds/worker -s /bin/bash -m worker
mkdir -p /builds/worker/artifacts
chown -R worker:worker /builds/worker
