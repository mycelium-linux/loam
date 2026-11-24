# Hello, Soil

A minimal web Spore for the Soil 101 walkthrough.

```sh
loam install examples/hello-soil --root ~/.mycelium/spores --allow-unsigned-dev
loamd serve --root ~/.mycelium/spores --web-listen 127.0.0.1:8799
curl -H 'Host: hello.myc' http://127.0.0.1:8799/
```

The manifest declares the web surface and the `hello.myc` anycast claim. The
page is static while the Soil compiler is a bootstrap; see
[`docs/soil-101.md`](../../docs/soil-101.md).
