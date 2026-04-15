# wp-knowledge Docs

`wp-knowledge/docs` 只保留语言入口，正文统一放在语言目录下：

- 中文文档入口：[docs/zh/README.md](./zh/README.md)
- English entry: [docs/en/README.md](./en/README.md)

## 目录约定

- `docs/zh/`
  - 中文文档与中文索引。
- `docs/en/`
  - English documents and English index.

正文分类在两个语言目录内保持一致：

- `guides/`
  - 面向使用者的说明、配置与上手文档。
- `architecture/`
  - 设计、边界、抽象和长期演进说明。
- `performance/`
  - 性能测试记录、对比与结论。
- `tasks/`
  - 阶段性任务拆分与内部推进文档。

## 维护约束

- 新文档必须放到 `docs/zh/` 或 `docs/en/` 之下，不再写入 `docs/` 根目录分类目录。
- 中英文文档尽量保持同名、同层级，便于双语同步和互相跳转。
- 每篇文档开头建议保留语言切换链接。
