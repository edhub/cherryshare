#!/usr/bin/env bash
# 编网页的静态文件到 web/dist/。产物不编进二进制，装的时候整目录拷过去就行。
set -euo pipefail
cd "$(dirname "$0")"

bun install
rm -rf dist
mkdir -p dist/_app
bunx @tailwindcss/cli -i input.css -o dist/_app/app.css --minify
cp index.html dist/index.html
cp app.js dist/_app/app.js

ls -1 dist dist/_app | sed 's/^/  /'
