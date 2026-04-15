# Docs Index

语言切换：

- 中文：当前目录
- English: [docs/en/README.md](../en/README.md)

`wp-knowledge/docs` 按内容类型拆成 4 类，并同时提供中英文入口。

## 中文

### Guides

- [KnowDB 配置说明](./guides/config.md)

### Architecture

- [Provider 与 Cache 架构说明](./architecture/provider-cache.md)

### Performance

- [Async Provider 性能测试与结论](./performance/async-provider.md)

### Tasks

- [PostgreSQL Provider Tasks](./tasks/postgres-provider.md)
- [Provider / Cache Refactor Tasks](./tasks/provider-cache.md)

## English

### Guides

- [KnowDB Configuration](../en/guides/config.md)

### Architecture

- [Provider and Cache Architecture](../en/architecture/provider-cache.md)

### Performance

- [Async Provider Performance](../en/performance/async-provider.md)

### Tasks

- [PostgreSQL Provider Tasks](../en/tasks/postgres-provider.md)
- [Provider / Cache Refactor Tasks](../en/tasks/provider-cache.md)

## 约定 / Conventions

- `guides/` 放对外使用说明和配置文档；`guides/` contains user-facing usage and configuration docs.
- `architecture/` 放设计、边界和长期演进说明；`architecture/` contains design notes, boundaries, and longer-term direction.
- `performance/` 放测试记录和性能结论；`performance/` contains benchmark records and conclusions.
- `tasks/` 放阶段性任务拆分，默认按内部工作文档看待；`tasks/` contains staged work plans and is usually treated as internal working documentation.
