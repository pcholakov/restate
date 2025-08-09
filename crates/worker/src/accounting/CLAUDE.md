# Storage Accounting Implementation Notes

This document captures key decisions and implementation details from the storage accounting feature development.

## Overview

Implements storage usage metrics for Restate Cloud billing:
- `restate.usage.state.storage_bytes` - Instantaneous total bytes stored (gauge)
- `restate.usage.state.storage_byte_seconds` - Cumulative byte-seconds over process lifetime (gauge)

## Key Technical Decisions

### 1. Metric Types (Overflow Protection)
**Problem**: Counter metrics only support `u64`, which overflows at ~7,000 GB-months
**Solution**: Use `gauge` for both metrics with `f64` values
- Gauges support `f64` via `IntoF64` trait
- Sufficient precision for storage accounting
- No overflow risk for realistic usage patterns

### 2. Byte-Seconds Calculation (Time-Weighted Averaging)
**Formula**: `(prev_bytes + curr_bytes) / 2 * time_diff_seconds`
**Why**: Provides accurate usage accounting between samples
**Edge case**: No calculation on first sample (no previous value)

### 3. SQL Query Optimization
```sql
SELECT sum(octet_length(key) + length(value)) as total_state_size FROM state
```
- `octet_length()` for UTF-8 key column (byte count)
- `length()` for binary value column (byte count)
- Handles both `UInt64Array/Int64Array` and `StringArray/LargeStringArray` result types

### 4. Under-Account Principle
- Log warnings but continue on query failures
- Set metrics to 0 when queries fail or no state exists
- Don't crash worker process on accounting failures
- Favor customers when uncertain (billing principle)

### 5. Architecture Choices
- **DataFusion integration**: Reuse existing query infrastructure vs direct RocksDB access
- **TaskCenter lifecycle**: Proper startup/shutdown with cancellation_watcher
- **Worker role integration**: Automatic startup when worker initializes
- **Update interval**: 1 second for testing (configurable, default would be 60s in production)

## Error Patterns Encountered

1. **HashMap imports**: Need `ahash::HashMap` with `HashMapExt` trait, not `std::collections::HashMap`
2. **Arrow array types**: Handle both `StringArray/LargeStringArray` and `UInt64Array/Int64Array` flexibly
3. **Borrow checker**: Clone `query_context` before moving `worker` in startup integration
4. **Metric API**: Counter uses `absolute(u64)`, Gauge uses `set(f64)`

## Testing Approach

Manual testing preferred over unit tests for this integration feature:
- Tests SQL query execution against real DataFusion context
- Validates metric updates in actual Prometheus environment
- Ensures proper startup/shutdown lifecycle behavior

## Code Style Notes

- Minimal comments (essential "why" only, not "what")
- Use `ahash::HashMap` for performance
- Follow existing codebase patterns for error handling and logging
- Debug logging for metric updates and byte-seconds calculations