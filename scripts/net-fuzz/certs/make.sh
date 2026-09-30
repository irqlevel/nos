#!/bin/sh
# The certificates net-fuzz's TLS server presents, made again: a CA the
# kernel's TLS client trusts in the fuzzer (the webpki-roots stand-in has
# ca.der as its one root), and P-256 leaves under it -- good.der for
# 10.0.2.100, the server's address, and three a client must refuse: one
# expired, one for another address, and a stranger no CA vouches for.
# ca-key.pem is the CA's key: a test key, and only here so that this script
# can sign with it. Run from anywhere; writes next to itself. Needs OpenSSL 3.
set -e
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
openssl x509 -inform DER -in ca.der -out "$tmp/ca.pem"

leaf() {
    openssl ecparam -name prime256v1 -genkey -noout -out "$tmp/$1.key"
    openssl req -new -key "$tmp/$1.key" -subj "/CN=$1" -out "$tmp/$1.csr"
    printf 'subjectAltName=%s\nextendedKeyUsage=serverAuth\nbasicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\n' \
        "$2" > "$tmp/$1.ext"
    openssl x509 -req -in "$tmp/$1.csr" -CA "$tmp/ca.pem" -CAkey ca-key.pem -set_serial "0x$(openssl rand -hex 8)" \
        -not_before "$3" -not_after "$4" -extfile "$tmp/$1.ext" -outform DER -out "$1.der"
    openssl pkcs8 -topk8 -nocrypt -in "$tmp/$1.key" -outform DER -out "$1-key.der"
}

leaf good "IP:10.0.2.100,DNS:fuzz.test" 20200101000000Z 21200101000000Z
leaf expired "IP:10.0.2.100,DNS:fuzz.test" 20200101000000Z 20210101000000Z
leaf other "IP:10.0.2.101,DNS:other.test" 20200101000000Z 21200101000000Z

openssl ecparam -name prime256v1 -genkey -noout -out "$tmp/stranger.key"
openssl req -new -x509 -key "$tmp/stranger.key" -subj "/CN=stranger" -days 36500 \
    -addext "subjectAltName=IP:10.0.2.100" -addext "extendedKeyUsage=serverAuth" -outform DER -out stranger.der
openssl pkcs8 -topk8 -nocrypt -in "$tmp/stranger.key" -outform DER -out stranger-key.der
