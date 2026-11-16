#!/bin/bash
# Produces small web versions of the raw art for the mock-up (art/web/).
set -eu
cd "$(dirname "$0")/../art"
mkdir -p web
for f in raw/*; do
  b=$(basename "$f"); id=${b%%_*}; kind=${b#*_}; kind=${kind%.*}
  case $kind in
    thumb) magick "$f[0]" -background '#11141b' -flatten -strip -quality 74 "web/${id}_thumb.jpg" ;;
    background)
      magick "$f[0]" -strip -resize '1280x>' -quality 50 "web/${id}_hero.webp"
      magick "$f[0]" -strip -resize 96x -blur 0x4 -quality 60 "web/${id}_amb.jpg" ;;
    logo) magick "$f[0]" -strip -trim +repage -resize '520x180>' -quality 82 -define webp:alpha-quality=80 "web/${id}_logo.webp" ;;
  esac
done
