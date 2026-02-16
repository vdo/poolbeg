# Meddler - Implementation Plan

## Overview

Meddler is a high-performance Web3 JSON-RPC caching proxy written in Rust. It sits between clients and upstream EVM nodes (Infura, Alchemy, Ankr, self-hosted), providing Redis-backed caching, WebSocket subscription aggregation, and upstream failover. Primary focus: Arbitrum (4 blocks/sec), with multi-chain support.

**Drop-in replacement**: Clients connect to meddler exactly as they would to any Web3 RPC endpoint. Both HTTP (`POST /arbitrum`) and WebSocket (`ws://meddler:8080/arbitrum`) are served on the same port. A client using ethers.js, viem, web3.py, or any Web3 library can point at meddler with zero code changes - including `eth_subscribe` over WebSocket.

## Architecture

```
          Clients (ethers.js, viem, web3.py, geth attach, etc.)
          ┌──────────────────────────────────────────────┐
          │  HTTP POST /arbitrum   WS ws:///arbitrum      │
          │  (JSON-RPC calls)      (JSON-RPC calls +      │
          │                         eth_subscribe)        │
          └──────────────┬───────────────┬───────────────┘
                         │               │
                   ┌─────▼───────────────▼─────┐
                   │  axum (single port 8080)   │
                   │  HTTP handler │ WS upgrade  │
                   └─────────────┬─────────────┘
                                 │
                   ┌─────────────▼─────────────┐
                   │     Cache Layer (Redis)    │
                   │  ┌─────────────────────┐   │
                   │  │   Block Tracker     │   │
                   │  │ (polls upstreams,   │   │
                   │  │  caches blocks,     │   │
                   │  │  detects reorgs)    │   │
                   │  └────────┬────────────┘   │
                   │           │ new block event │
                   │  ┌────────▼────────────┐   │
                   │  │ Subscription Manager│   │
                   │  │ (reads from cache,  │   │
                   │  │  fans out to WS     │   │
                   │  │  clients)           │   │
                   │  └─────────────────────┘   │
                   └─────────────┬─────────────┘
                                 │
                   ┌─────────────▼─────────────┐
                   │    Upstream Manager        │
                   │  primary → secondary →     │
                   │           fallback         │
                   └──┬──────┬──────┬──────┬───┘
                      │      │      │      │
                   +--▼-+ +--▼-+ +--▼-+ +--▼-+
                   │node│ │node│ │infr│ │alch│
                   │  1 │ │  2 │ │ura │ │emy │
                   +----+ +----+ +----+ +----+
```

## Tech Stack

| Component | Crate | Why |
|---|---|---|
| HTTP server | `axum` + `tower` | Tower middleware for cache/proxy layers, built-in WS |
| WebSocket (server) | `axum::extract::ws` | Native axum support, client-facing WS |
| Redis | `fred` | Connection pooling, pub/sub, reconnect, pipeline |
| JSON-RPC types | `alloy-json-rpc` + `alloy-rpc-types-eth` | Canonical EVM types, replaces deprecated ethers |
| EVM primitives | `alloy-primitives` | `U256`, `B256`, `Address` |
| Async runtime | `tokio` (multi-thread) | Standard for Rust async |
| Config | `figment` + `serde` + `serde_yaml` | YAML config with env var overrides |
| CLI | `clap` | `--config`, `--log-level` |
| Logging | `tracing` + `tracing-subscriber` | Structured async-aware logging |
| Metrics | `metrics` + `metrics-exporter-prometheus` | `/metrics` endpoint |
| Hashing (cache keys) | `blake3` | Fast cache key generation |
| Serialization | `serde` + `serde_json` | JSON-RPC ser/de |

## Configuration (YAML)

```yaml
server:
  address: "0.0.0.0"
  port: 8080            # single port: HTTP + WS upgrade on same port
  # Clients connect exactly like a normal Web3 RPC:
  #   HTTP:  curl -X POST http://meddler:8080/arbitrum -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
  #   WS:    wscat -c ws://meddler:8080/arbitrum
  #          > {"jsonrpc":"2.0","method":"eth_subscribe","params":["newHeads"],"id":1}
  #          < {"jsonrpc":"2.0","result":"0x1","id":1}
  #          < {"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0x1","result":{...}}}
  metrics:
    address: "0.0.0.0"
    port: 9090
    path: "/metrics"

cache:
  redis:
    url: "redis://localhost:6379"
    pool_size: 16
    db: 0
  policies:
    # finalized data = cache permanently (TTL 0 means no expiry)
    - methods: ["eth_getBlockByHash", "eth_getTransactionByHash", "eth_getTransactionReceipt"]
      finality: finalized
      ttl: 0
    # unfinalized/latest = short TTL
    - methods: ["eth_getBlockByNumber", "eth_getBalance", "eth_getCode"]
      finality: unfinalized
      ttl: 2s
    # never cache
    - methods: ["eth_sendRawTransaction", "eth_estimateGas", "eth_call"]
      finality: any
      ttl: -1    # -1 = never cache
    # default catch-all
    - methods: ["*"]
      finality: finalized
      ttl: 3600s
    - methods: ["*"]
      finality: unfinalized
      ttl: 3s

chains:
  - name: arbitrum
    chain_id: 42161
    # block time used for health checks and subscription polling
    expected_block_time: 250ms
    # how many blocks behind finalized to consider "finalized"
    finality_depth: 100
    route: /arbitrum
    upstreams:
      - id: infura-arb
        role: primary
        http_url: "https://arbitrum-mainnet.infura.io/v3/${INFURA_KEY}"
        ws_url: "wss://arbitrum-mainnet.infura.io/ws/v3/${INFURA_KEY}"
        max_rps: 50
      - id: public-arb
        role: fallback
        http_url: "https://arb1.arbitrum.io/rpc"
        max_rps: 10

  - name: ethereum
    chain_id: 1
    expected_block_time: 12s
    finality_depth: 64
    route: /ethereum
    upstreams:
      - id: infura-eth
        role: primary
        http_url: "https://mainnet.infura.io/v3/${INFURA_KEY}"
        ws_url: "wss://mainnet.infura.io/ws/v3/${INFURA_KEY}"
        max_rps: 50
```

## Implementation Phases

---

### Phase 1: Project Skeleton & Config

**Goal**: Runnable binary that parses config and starts an axum server.

- `cargo init` with workspace structure
- Define config structs with `serde` + `figment`
- Parse YAML config with env var interpolation (`${VAR}` syntax)
- CLI with `clap`: `--config <path>`, `--log-level`
- axum server skeleton with health check (`/health`) and metrics (`/metrics`)
- `tracing` setup with JSON and pretty formatters
- **Files**: `Cargo.toml`, `src/main.rs`, `src/config.rs`, `src/server.rs`

### Phase 2: Upstream Manager, Block Tracker & Health Checks

**Goal**: Connect to upstream nodes, health check them, track chain head, route requests by role tier.

- `UpstreamManager`: manages a set of upstreams per chain
- Each `Upstream` has: id, role (primary/secondary/fallback), HTTP client (`reqwest`), health status, current block height
- Health check loop: periodically call `eth_blockNumber` + `net_peerCount` on each upstream
- Request routing: try primary first, fall back to secondary, then fallback
- Per-upstream rate limiting (token bucket)
- **Block Tracker** per chain (the single source of truth for chain head):
  - Polls `eth_getBlockByNumber("latest")` on the best available upstream at `expected_block_time` interval (250ms for Arbitrum)
  - Also polls `eth_getBlockByNumber("finalized")` at a slower interval
  - Caches every new block header + full block in Redis
  - Fetches `eth_getLogs` for each new block and caches the logs in Redis
  - Emits internal events via `tokio::sync::broadcast`: `NewBlock(header)`, `NewLogs(logs)`, `Reorg(from, to)`
  - Detects reorgs: if new block number <= previous but different hash, emit `Reorg` event
  - These events drive both cache invalidation (Phase 4) and WS subscriptions (Phase 5)
- **Files**: `src/upstream/mod.rs`, `src/upstream/manager.rs`, `src/upstream/health.rs`, `src/upstream/client.rs`, `src/upstream/tracker.rs`

### Phase 3: HTTP JSON-RPC Proxy (no cache)

**Goal**: Forward JSON-RPC requests to upstreams and return responses.

- Route: `POST /{chain}` (e.g., `/arbitrum`, `/ethereum`) - same route will later accept WS upgrade in Phase 5
- Parse JSON-RPC request (single + batch)
- Normalize request ID for cache key generation (set to `0`)
- Forward to best available upstream via UpstreamManager
- Return JSON-RPC response with original request ID restored
- Error handling: upstream timeout, connection error, JSON-RPC error
- CORS support
- **Files**: `src/rpc/mod.rs`, `src/rpc/handler.rs`, `src/rpc/types.rs`

### Phase 4: Redis Cache Layer

**Goal**: Cache JSON-RPC responses in Redis with finality-aware TTLs. The cache is the single source of truth - both HTTP responses and WS subscriptions read from it.

- Cache key: `meddler:{chain_id}:{blake3(normalized_request)}`
- On request: check Redis first; on hit, return cached response
- On miss: forward to upstream, cache response based on policy
- **Finality-aware caching**:
  - Uses Block Tracker's `latest_block` and `finalized_block` (from Phase 2)
  - Requests referencing `"latest"`, `"pending"`, `"safe"` = unfinalized (short TTL)
  - Requests referencing block hash or number <= finalized = finalized (long TTL)
  - `eth_getBlockByNumber("finalized")` resolves the finalized block number first
- **Block Tracker feeds the cache proactively**:
  - Every new block header from the tracker → cached under `eth_getBlockByNumber` / `eth_getBlockByHash`
  - Every new set of logs from the tracker → cached under `eth_getLogs` for that block range
  - This means `newHeads` / `logs` subscription data is already in Redis before any client asks for it
- **Head cache**: track Redis keys for unfinalized responses
  - On `NewBlock` event: mark previous "latest" keys as stale
  - On `Reorg` event: delete all head cache entries back to the fork point
- Cache bypass for uncacheable methods (`eth_sendRawTransaction`, `eth_estimateGas`, `eth_call`)
- Redis pipeline for batch requests
- **Files**: `src/cache/mod.rs`, `src/cache/redis.rs`, `src/cache/policy.rs`, `src/cache/keys.rs`

### Phase 5: Client-Facing WebSocket & Cache-Driven Subscriptions

**Goal**: Full Web3-compatible WebSocket interface. Clients connect to meddler via WS and use it exactly like a real node. Subscriptions are served entirely from the cache/block tracker - no direct upstream WS passthrough.

**Client-facing WS interface** (same port as HTTP, axum WS upgrade):
- Route: `GET /{chain}` with `Upgrade: websocket` header (standard WS handshake)
- Clients connect with any Web3 library: `new ethers.WebSocketProvider("ws://meddler:8080/arbitrum")`
- **Full JSON-RPC over WS**: clients can send any JSON-RPC call over the WS connection (not just subscriptions). These go through the same cache layer as HTTP requests.
- **`eth_subscribe` support** - clients subscribe exactly like they would against a real node:
  - `eth_subscribe("newHeads")` - new block headers
  - `eth_subscribe("logs", {filter})` - filtered event logs
  - `eth_subscribe("newPendingTransactions")` - pending txs (if upstream supports)
  - `eth_subscribe("syncing")` - sync status changes
- **`eth_unsubscribe`** - standard unsubscribe
- Each client gets unique subscription IDs (hex string), matching the Ethereum JSON-RPC spec
- Batch JSON-RPC requests over WS are supported

**Cache-driven subscription backend** (no direct upstream WS per subscription):
- **Subscription Manager** per chain listens to Block Tracker's `tokio::sync::broadcast` events (from Phase 2):
  - `NewBlock(header)` → reads full block header from Redis cache → pushes to all `newHeads` subscribers
  - `NewLogs(block_number)` → reads logs from Redis cache → filters per client's log filter → pushes matching logs to `logs` subscribers
  - `Reorg(from, to)` → notifies subscribed clients of the reorg (re-sends new chain of headers)
- **No upstream WS connections needed for subscriptions** - the Block Tracker already polls upstreams via HTTP and caches the results. Subscriptions simply read from that cached data and fan out.
- This means subscriptions are:
  - **Cache-consistent**: clients see the exact same data that HTTP `eth_getBlockByNumber` would return
  - **Scalable**: 10,000 WS clients don't mean 10,000 upstream connections - just one polling loop per chain
  - **Resilient**: upstream failover is handled by the Block Tracker, completely invisible to subscription clients

**Client connection lifecycle**:
1. Client opens WS to `ws://meddler:8080/arbitrum`
2. Client sends `{"jsonrpc":"2.0","method":"eth_subscribe","params":["newHeads"],"id":1}`
3. Meddler responds: `{"jsonrpc":"2.0","result":"0xabc123","id":1}` (subscription ID)
4. Block Tracker detects new block → caches in Redis → emits `NewBlock` event
5. Subscription Manager reads header from cache → pushes to client: `{"jsonrpc":"2.0","method":"eth_subscription","params":{"subscription":"0xabc123","result":{...block header...}}}`
6. Client can also send regular RPC: `{"jsonrpc":"2.0","method":"eth_getBalance","params":["0x...","latest"],"id":2}` → cached response
7. Client sends `eth_unsubscribe("0xabc123")` or disconnects → cleanup

- **Files**: `src/ws/mod.rs`, `src/ws/handler.rs`, `src/ws/subscription.rs`

### Phase 6: Metrics & Observability

**Goal**: Prometheus metrics and structured logging.

- Request metrics: `meddler_requests_total{chain, method, status, cache_hit}`
- Latency: `meddler_request_duration_seconds{chain, method, upstream}`
- Upstream health: `meddler_upstream_healthy{chain, upstream_id}`
- Upstream block height: `meddler_upstream_block_height{chain, upstream_id}`
- Cache metrics: `meddler_cache_hits_total`, `meddler_cache_misses_total`, `meddler_cache_size_bytes`
- WS metrics: `meddler_ws_active_connections{chain}`, `meddler_ws_active_subscriptions{chain, subscription_type}`
- Redis metrics: `meddler_redis_pool_active`, `meddler_redis_latency_seconds`
- Prometheus endpoint: `GET /metrics`
- **Files**: `src/metrics.rs`

### Phase 7: Docker & CI

**Goal**: Multi-stage Docker build and GitHub Actions workflows.

- `Dockerfile`: multi-stage (rust builder → distroless/scratch runtime)
- `.github/workflows/build.yml`: lint, test, build
- `.github/workflows/docker.yml`: build + push to `ghcr.io`
- `.github/workflows/release.yml`: tagged releases
- **Files**: `Dockerfile`, `.github/workflows/`

### Phase 8: Helm Chart

**Goal**: Kubernetes deployment with Redis sidecar.

- Chart structure: `charts/meddler/`
- Deployment with meddler container + Redis sidecar (like dshackle)
- ConfigMap for `config.yaml`
- Secret for upstream API keys
- Service + Ingress
- ServiceMonitor for Prometheus scraping
- HPA based on CPU/request rate
- Values: image tag, replica count, resource limits, Redis config, meddler config overrides
- **Files**: `charts/meddler/`

---

## Project Structure

```
meddler/
├── Cargo.toml
├── Cargo.lock
├── config.example.yaml
├── Dockerfile
├── CLAUDE.md
├── PLAN.md
├── .github/
│   └── workflows/
│       ├── ci.yml
│       ├── docker.yml
│       └── release.yml
├── charts/
│   └── meddler/
│       ├── Chart.yaml
│       ├── values.yaml
│       └── templates/
│           ├── deployment.yaml
│           ├── service.yaml
│           ├── configmap.yaml
│           ├── secret.yaml
│           ├── ingress.yaml
│           ├── servicemonitor.yaml
│           └── hpa.yaml
└── src/
    ├── main.rs              # CLI + bootstrap
    ├── config.rs            # Config structs + parsing
    ├── server.rs            # axum server setup + routing
    ├── metrics.rs           # Prometheus metrics
    ├── rpc/
    │   ├── mod.rs
    │   ├── handler.rs       # HTTP JSON-RPC request handler
    │   └── types.rs         # JSON-RPC types (request, response, batch)
    ├── cache/
    │   ├── mod.rs
    │   ├── redis.rs         # Redis operations
    │   ├── policy.rs        # Cache policy evaluation
    │   └── keys.rs          # Cache key generation (blake3)
    ├── upstream/
    │   ├── mod.rs
    │   ├── manager.rs       # Upstream selection + failover
    │   ├── health.rs        # Health check loop
    │   ├── client.rs        # HTTP client per upstream
    │   └── tracker.rs       # Block Tracker: polls head, caches blocks/logs, emits events
    └── ws/
        ├── mod.rs
        ├── handler.rs       # WS connection handler (upgrade, per-client msg loop)
        └── subscription.rs  # Subscription manager (listens to tracker events, fans out to clients)
```

## Key Design Decisions

1. **Cache key = `blake3(normalized_json_rpc_request)`** - Normalize by setting `id: 0` and sorting params canonically. blake3 is ~3x faster than SHA-256.

2. **Finality tracking** - Each chain tracks `latest_block` and `finalized_block` via upstream health checks. Cache TTLs are determined by whether the requested data falls before or after the finalized block.

3. **Cache-driven WS subscriptions** - Subscriptions don't maintain separate upstream WS connections. The Block Tracker polls upstreams via HTTP, caches blocks/logs in Redis, and emits events. The Subscription Manager reads from cache and fans out to WS clients. This means subscriptions are always cache-consistent, horizontally scalable, and resilient to upstream failures.

4. **Role-based upstream tiers** - `primary` → `secondary` → `fallback`. Simple to reason about operationally. Within a tier, requests round-robin. If all upstreams in a tier are unhealthy, fall to the next tier.

5. **Redis-only cache** - No embedded cache. Redis is required (sidecar in k8s). This keeps the binary stateless and horizontally scalable.

6. **Env var interpolation in config** - `${INFURA_KEY}` in YAML gets replaced from environment. Supports `${VAR:-default}` syntax. Keeps secrets out of config files.

## Non-Goals (for now)

- gRPC interface (dshackle has it, but JSON-RPC is the standard client interface)
- Consensus policy / hedge requests (eRPC features - can add later)
- Archive node routing by label (can add later)
- Admin API (can add later)
- TLS termination (delegate to ingress/nginx)
