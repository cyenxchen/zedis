#!/bin/bash

set -euo pipefail

if [ $# -lt 2 ]; then
    echo "Usage: upload_asset.sh <FILE> <TOKEN>"
    exit 1
fi

repo="${GITHUB_REPOSITORY:-cyenxchen/zedis}"
file_path=$1
bearer=$2
file_name=${file_path##*/}

if [ ! -f "$file_path" ]; then
    printf "\e[31mError: File not found: %s\e[0m\n" "$file_path"
    exit 1
fi

echo "Starting asset upload from $file_path to $repo."

tag="$(git describe --tags --abbrev=0)"
if [ -z "$tag" ]; then
    printf "\e[31mError: Unable to find git tag\e[0m\n"
    exit 1
fi

echo "Git tag: $tag"

# Reuse the Actions token so gh can create/upload releases across reruns.
export GH_TOKEN="${GH_TOKEN:-$bearer}"

echo "Checking for existing release..."
if ! gh release view "$tag" --repo "$repo" >/dev/null 2>&1; then
    echo "No release found."
    echo "Creating new release..."
    gh release create "$tag" --repo "$repo" --draft --title "$tag" >/dev/null
fi

echo "Uploading asset $file_name..."
gh release upload "$tag" "$file_path" --repo "$repo" --clobber

printf "\e[32mSuccess\e[0m\n"
