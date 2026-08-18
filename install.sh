#!/usr/bin/env bash
# Instala o binário `local-proxy` a partir do último release do GitHub.
# Uso: ./install.sh [TAG]
set -euo pipefail

REPO="${LOCAL_PROXY_REPO:-gsporto226/local-proxy}"
TAG="${1:-}"
INSTALL_DIR="${LOCAL_PROXY_INSTALL_DIR:-$HOME/.local/bin}"

os="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$os" in
  linux*)  os="linux" ;;
  darwin*) os="darwin" ;;
  *)       echo "SO não suportado: $os" >&2; exit 1 ;;
esac

arch="$(uname -m)"
case "$arch" in
  x86_64|amd64) arch="x86_64" ;;
  *) echo "Arquitetura não suportada: $arch (apenas x86_64)" >&2; exit 1 ;;
esac

bin="$os-local-proxy"
if [ "$os" = "linux" ]; then bin="local-proxy"; fi

if [ -n "$TAG" ]; then
  release_url="https://api.github.com/repos/$REPO/releases/tags/$TAG"
else
  release_url="https://api.github.com/repos/$REPO/releases/latest"
fi

echo "> Buscando release: $release_url"
release="$(curl -fsSL -H "Accept: application/vnd.github+json" -H "User-Agent: local-proxy-installer" "$release_url")"
tag="$(printf '%s' "$release" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -n1)"

asset_url="$(printf '%s' "$release" | tr '{' '\n' | grep -m1 "\"name\": *\"$bin\"" | sed -n 's/.*"browser_download_url": *"\([^"]*\)".*/\1/p')"
if [ -z "$asset_url" ]; then
  echo "Binário '$bin' não encontrado no release $tag" >&2
  exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "> Baixando $bin ($tag)..."
curl -fsSL -o "$tmp/$bin" "$asset_url"

if [ "${LOCAL_PROXY_SKIP_VERIFY:-}" != "1" ]; then
  echo "> Verificando SHA256..."
  curl -fsSL -o "$tmp/$bin.sha256" "$asset_url.sha256"
  expected="$(awk '{print $1}' "$tmp/$bin.sha256" | tr '[:upper:]' '[:lower:]')"
  actual="$(sha256sum "$tmp/$bin" | awk '{print $1}')"
  if [ "$expected" != "$actual" ]; then
    echo "SHA256 não confere. esperado=$expected obtido=$actual" >&2
    exit 1
  fi
  echo "  OK: $actual"
fi

mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp/$bin" "$INSTALL_DIR/$bin"

echo
echo "Instalado: $INSTALL_DIR/$bin"
if ! command -v local-proxy >/dev/null 2>&1 && ! echo ":$PATH:" | grep -q ":$(cd "$INSTALL_DIR" && pwd):"; then
  echo "> Para usar, adicione ao PATH:"
  echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
fi
