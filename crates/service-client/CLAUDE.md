# Service Client CLAUDE.md

Service client implementation notes for Claude Code.

## HTTP Version Handling in Discovery and Invocation

### Overview
Restate uses different HTTP versions for service discovery and invocation, with specific handling for HTTP/1.1 vs HTTP/2. Understanding the relationship between HTTP versions and Restate's invocation protocols is crucial for debugging connection issues.

### HTTP Client Architecture (`src/http.rs`)

The `HttpClient` implements a dual-client architecture:

- **`client`**: Handles HTTPS and HTTP/1.1 with h2c upgrade capability
- **`h2c_prior_knowledge_client`**: Handles HTTP/2 with prior knowledge over plain HTTP

**Version Selection Logic**:
```rust
match (request.version(), request.uri().scheme()) {
    (Version::HTTP_2, scheme) if scheme == &Scheme::HTTP => {
        // Use h2c prior knowledge client for HTTP/2 over plain HTTP
        self.h2c_prior_knowledge_client.request(request)
    }
    (_, _) => self.client.request(request),
}
```

### Discovery Behavior

**Default Discovery Strategy**:
- Uses HTTP/2 with prior knowledge for HTTP endpoints
- Falls back gracefully when services don't support HTTP/2

**--use-http1.1 Flag**:
- Forces HTTP/1.1 usage during discovery
- Primarily needed for local development servers (e.g., `wrangler dev`)
- Should not be needed in production environments

### Error Detection and Recovery

The HTTP client can detect version compatibility issues:

- **`PossibleHTTP11Only`**: Detects H2 frame size errors indicating server only supports HTTP/1.1
- **`PossibleHTTP2Only`**: Detects "invalid HTTP version parsed" errors
- **META0014 Error**: Maps to `PossibleHTTP11Only` errors during discovery

### Restate Protocol Types vs HTTP Versions

**Important Distinction**: HTTP version (transport) ≠ Restate protocol type (semantics)

**Protocol Types**:
- `RequestResponse`: Traditional request/response pattern
- `BidiStream`: Bidirectional streaming for advanced workflows

**HTTP Version Compatibility Matrix**:
| HTTP Version | RequestResponse | BidiStream |
|--------------|----------------|------------|
| HTTP/1.1     | ✅ Always      | ⚠️ Server-dependent |
| HTTP/2+      | ✅ Always      | ✅ Always |
| Lambda       | ✅ Always      | ❌ Never  |

**Default Version Mapping**:
- `BidiStream` → HTTP/2 (requires streaming capabilities)
- `RequestResponse` → HTTP/1.1 (simpler, more compatible)

### HTTP/1.1 Bidirectional Streaming Requirements

HTTP/1.1 bidirectional streaming is **server-dependent** because it relies on careful implementation rather than native protocol support. The code comments indicate Restate takes a "trust the user" approach - if a service advertises `BidiStream` support over HTTP/1.1 during discovery, Restate will attempt to use it.

**Technical Requirements for HTTP/1.1 Bidi**:

1. **Server Implementation**:
   - Must support `Transfer-Encoding: chunked` for streaming responses
   - Must maintain persistent connections (`Connection: keep-alive`)
   - Should handle request/response pipelining properly
   - Must flush responses immediately without buffering

2. **Infrastructure Requirements**:
   - Load balancers must not buffer or modify streaming behavior
   - Proxies must pass through chunked encoding and maintain connection state
   - CDNs/Edge services must support HTTP/1.1 streaming without interference

3. **SDK Implementation**:
   - Restate SDK must properly implement bidirectional protocol over HTTP/1.1
   - Must handle concurrent message sending/receiving
   - Must manage connection state correctly

**Why HTTP/1.1 Bidi is "Server-Dependent"**:
Unlike HTTP/2 which has native multiplexing and streaming, HTTP/1.1 bidirectional streaming is essentially a workaround that relies on chunked encoding, connection persistence, careful timing, and no intermediate buffering. This makes it fragile and dependent on the entire network path supporting these requirements.

**Recommendation**: Use HTTP/2 for bidirectional streaming when possible. HTTP/1.1 bidi can work but requires careful configuration of the entire network stack.

### Troubleshooting Connection Issues

1. **META0014 Errors**: Try discovery with `--use-http1.1` flag
2. **Local Development**: Many local dev servers only support HTTP/1.1
3. **Production Deployments**: Should generally work with default HTTP/2 discovery
4. **Bidirectional Services**: Ensure endpoint supports HTTP/2 or has proper HTTP/1.1 streaming support

### Key Files
- `src/http.rs`: HTTP client implementation and version selection
- `../service-protocol/src/discovery.rs`: Discovery protocol implementation  
- `../types/src/schema/deployment.rs`: Protocol type definitions
- `../../cli/src/commands/deployments/register.rs`: CLI flag handling

### Common Patterns
- Always prefer HTTP/2 for new deployments when possible
- Use `--use-http1.1` only for local development compatibility
- Bidirectional protocols work best with HTTP/2
- Lambda deployments are always request/response only