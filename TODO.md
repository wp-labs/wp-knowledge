# TODO

- 明确 MySQL UUID 语义策略：
  - `CHAR/VARCHAR(36)` UUID 是否始终按文本返回
  - `BINARY(16)` UUID 是否显式解码为 UUID，还是继续按 `0x...` 返回
- 补 provider 类型覆盖：
  - Postgres: `UUID[]`、`INET[]`、`CIDR[]`、`MACADDR`
  - MySQL: UUID 文本/二进制兼容测试
- 增强 provider decode 错误信息：
  - 带上列名、列索引、数据库类型名
- 优化 correctness 脚本执行时间：
  - 减少重复 `cargo test` 编译开销
- 补一页类型映射文档：
  - 记录 MySQL/Postgres 当前支持类型及 `DataField` 返回语义
