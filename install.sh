#!/bin/bash
set -e

urlencode() {
    old_lc_collate=$LC_COLLATE
    LC_COLLATE=C
    local length="${#1}"
    for (( i = 0; i < length; i++ )); do
        local c="${1:$i:1}"
        case $c in
            [a-zA-Z0-9.~_-]) printf '%s' "$c" ;;
            *) printf '%%%02X' "'$c" ;;
        esac
    done
    LC_COLLATE=$old_lc_collate
}

GITHUB_USER="INTERLIGENT-kommunzieren-GmbH"
PROJECT="docker-plugin"
REPO="$GITHUB_USER/$PROJECT"
INSTALL_DIR="$HOME/.ik/docker"
DOCKER_CLI_PLUGIN_PATH="$HOME/.docker/cli-plugins"

# GitHub API: get latest release tag
LATEST_TAG=$(curl -s "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name":' | cut -d '"' -f4)

ARCHIVE_URL="https://github.com/${REPO}/archive/refs/tags/${LATEST_TAG}.tar.gz"

mkdir -p "$INSTALL_DIR"
cd "$INSTALL_DIR"
rm -rf ./*

curl -sL "$ARCHIVE_URL" | tar xz --strip-components=1

mkdir -p "$DOCKER_CLI_PLUGIN_PATH"
if [[ -f "$DOCKER_CLI_PLUGIN_PATH/docker-control" ]]; then
    echo "Removing old plugin under $DOCKER_CLI_PLUGIN_PATH/docker-control"
    rm "$DOCKER_CLI_PLUGIN_PATH/docker-control"
fi

echo "Installing plugin under $DOCKER_CLI_PLUGIN_PATH"
ln -s "$INSTALL_DIR/plugin/docker-control" "$DOCKER_CLI_PLUGIN_PATH/docker-control"
chmod 755 "$DOCKER_CLI_PLUGIN_PATH/docker-control"

echo "Installation successful. You can start using the plugin with: docker control help"
