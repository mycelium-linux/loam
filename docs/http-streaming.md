# Outbound HTTP streaming

`soil:http@0.2.0` has two response paths.

## Buffered

```wit
fetch: func(request: request) -> result<response, error>;
```

Use this for ordinary request/response calls. The host enforces the body cap
before returning.

## Streaming

```wit
open: func(request: request) -> result<response-stream, error>;

resource response-stream {
  status: func() -> u16;
  final-url: func() -> string;
  headers: func() -> list<header>;
  read: func(max: u32) -> result<option<list<u8>>, error>;
  close: func() -> result<_, error>;
}
```

`open` returns after the response headers, then the guest pulls the body:

```text
status/headers available
  -> read(max)  -> chunk
  -> read(max)  -> chunk
  -> read(max)  -> none (EOF)
  -> close
```

The same origin, method, header, credential and budget policy applies to both
`fetch` and `open`. `read` accumulates against the request's `max-bytes`; a body
that exceeds it returns `over-budget`.

## Credentials

The guest supplies an opaque credential handle in `request.credential`. The host
resolves it and injects the header. The guest never sees a token, key, username
or password. The allowlist and credential resolver are host policy; a grant
manifest chooses which handles and origins a Spore may use.

## Why not `soil:stream`?

`soil:stream` is a host-issued **inbound** duplex session: the host accepts the
transport, authenticates the peer, and queues it for a Spore. It is not a
client-side HTTP response body. Reusing it for outbound HTTP would lose status,
headers, redirect policy, credential injection and the HTTP budget. Outbound
body streaming belongs to `soil:http`, where those semantics live.

## Inference consequence

An inference provider Plugin can now speak SSE through:

```text
soil:http.open -> response-stream.read -> parse SSE -> soil:inference completion-stream
```

The plugin remains a Spore package exporting WIT; no Loam host capability is
required for the transport. The only Loam-side requirement is that the consumer
grant `soil:http` and the host has an allowlisted origin plus credential handle.
