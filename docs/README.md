# 文档索引

本索引列出 Xet Server 项目的所有文档。

## 核心文档

### 项目文档
- **[README.md](../README.md)** - 项目概述、快速开始、使用示例
- **[HF_XET_INTEGRATION_GUIDE.md](../HF_XET_INTEGRATION_GUIDE.md)** - HuggingFace 集成指南
- **[HUB_UPLOAD_DOWNLOAD_TEST_REPORT.md](../HUB_UPLOAD_DOWNLOAD_TEST_REPORT.md)** - Hub 上传/下载测试报告

### 用户指南
- **[配置指南](configuration.md)** - 完整的配置选项说明
- **[架构文档](architecture.md)** - 系统架构和数据流说明

## API 文档

### API 参考
- **[CAS API](api/cas-api.md)** - CAS 服务器 API 详细参考
- **[Hub API](api/hub-api.md)** - Hub API 详细参考
- **[认证文档](api/authentication.md)** - Ed25519 JWT 认证机制

## 设计文档

### 已完成的设计规范

#### 2026-06-09: Metrics Dead Code Fix
- **设计规范**: [specs/2026-06-09-metrics-dead-code-fix-design.md](superpowers/specs/2026-06-09-metrics-dead-code-fix-design.md) ✅
- **实施计划**: [plans/2026-06-09-metrics-dead-code-fix.md](superpowers/plans/2026-06-09-metrics-dead-code-fix.md) ✅
- **状态**: 已完成（2026-06-11）
- **描述**: 激活未使用的 metrics API 方法（connection_opened/closed, record_download_bytes）

#### 2026-06-09: Xet Server HF Testing
- **设计规范**: [specs/2026-06-09-xet-server-hf-testing-design.md](superpowers/specs/2026-06-09-xet-server-hf-testing-design.md) ✅
- **实施计划**: [plans/2026-06-09-xet-server-hf-testing.md](superpowers/plans/2026-06-09-xet-server-hf-testing.md) ✅
- **状态**: 已完成（2026-06-11）
- **描述**: 使用 HuggingFace 命令测试 Xet Server 的上传/下载功能

#### 2026-06-10: HuggingFace Hub API
- **设计规范**: [specs/2026-06-10-hf-hub-api-design.md](superpowers/specs/2026-06-10-hf-hub-api-design.md) ✅
- **CAS 修改计划**: [plans/2026-06-10-cas-modifications.md](superpowers/plans/2026-06-10-cas-modifications.md) ✅
- **Hub API 计划**: [plans/2026-06-10-hub-api-service.md](superpowers/plans/2026-06-10-hub-api-service.md) ✅
- **状态**: 已完成（2026-06-12）
- **描述**: 实现 HuggingFace Hub REST API 兼容层

## 文档结构

```
/data/
├── README.md                              # 项目主文档
├── HF_XET_INTEGRATION_GUIDE.md           # HuggingFace 集成指南
├── HUB_UPLOAD_DOWNLOAD_TEST_REPORT.md    # 测试报告
├── async_sqlite_migration_analysis.md    # SQLite 异步迁移分析（已完成）
├── cas_sqlite_dependency_analysis.md     # CAS SQLite 依赖分析（已完成）
│
├── docs/
│   ├── README.md                         # 本文档索引
│   ├── configuration.md                  # 配置指南
│   ├── architecture.md                   # 架构文档
│   ├── DOCUMENTATION_SYNC_COMPLETION_REPORT.md  # 文档同步完成报告
│   ├── CAS_STATE_DB_PATH_ROLLBACK.md     # CAS_STATE_DB_PATH 回滚记录
│   ├── CONFIG_FIXES_PHASE1.md            # Phase 1 配置修复记录
│   │
│   ├── api/                              # API 文档
│   │   ├── cas-api.md                    # CAS API 参考
│   │   ├── hub-api.md                    # Hub API 参考
│   │   └── authentication.md             # 认证文档
│   │
│   └── superpowers/                      # 设计和计划文档
│       ├── specs/                        # 设计规范
│       │   ├── 2026-06-09-metrics-dead-code-fix-design.md
│       │   ├── 2026-06-09-xet-server-hf-testing-design.md
│       │   └── 2026-06-10-hf-hub-api-design.md
│       │
│       └── plans/                        # 实施计划
│           ├── 2026-06-09-metrics-dead-code-fix.md
│           ├── 2026-06-09-xet-server-hf-testing.md
│           ├── 2026-06-10-cas-modifications.md
│           └── 2026-06-10-hub-api-service.md
```

## 分析报告

### 已完成的技术分析

- **[SQLite 异步迁移分析](../async_sqlite_migration_analysis.md)** - TokenStore 从 rusqlite 迁移到 sqlx（✅ 已完成 2026-06-13，commit 1486507）
- **[CAS SQLite 依赖分析](../cas_sqlite_dependency_analysis.md)** - CAS 移除 SQLite 依赖（✅ 已完成 2026-06-13，commit 2491af1）

### 修复记录

- **[文档同步完成报告](DOCUMENTATION_SYNC_COMPLETION_REPORT.md)** - 60+ 文档问题修复记录（2026-06-12）
- **[CAS_STATE_DB_PATH 回滚记录](CAS_STATE_DB_PATH_ROLLBACK.md)** - 删除不存在的配置项（2026-06-12）
- **[Phase 1 配置修复记录](CONFIG_FIXES_PHASE1.md)** - 端口冲突、阈值不匹配等修复（2026-06-12）

## 快速导航

### 新用户
1. 阅读 [README.md](../README.md) 了解项目概述
2. 按照快速开始指南安装和配置
3. 查看 [HF_XET_INTEGRATION_GUIDE.md](../HF_XET_INTEGRATION_GUIDE.md) 了解使用方式

### 开发者
1. 阅读 [架构文档](architecture.md) 了解系统设计
2. 查看 [API 文档](api/) 了解接口细节
3. 参考 [配置指南](configuration.md) 进行配置

### 运维人员
1. 阅读 [配置指南](configuration.md) 了解所有配置选项
2. 查看安全考虑和最佳实践
3. 参考监控和日志部分

### 贡献者
1. 阅读 [架构文档](architecture.md) 了解系统架构
2. 查看已完成的设计文档了解设计决策
3. 遵循代码规范和测试要求

## 文档状态

| 文档 | 状态 | 最后更新 |
|------|------|----------|
| README.md | ✅ 完成 | 2026-06-13 |
| HF_XET_INTEGRATION_GUIDE.md | ✅ 完成 | 2026-06-12 |
| 配置指南 | ✅ 完成 | 2026-06-13 |
| 架构文档 | ✅ 完成 | 2026-06-13 |
| CAS API 文档 | ✅ 完成 | 2026-06-12 |
| Hub API 文档 | ✅ 完成 | 2026-06-12 |
| 认证文档 | ✅ 完成 | 2026-06-12 |
| Metrics 设计 | ✅ 完成 | 2026-06-11 |
| HF Testing 设计 | ✅ 完成 | 2026-06-11 |
| Hub API 设计 | ✅ 完成 | 2026-06-12 |
| SQLite 迁移分析 | ✅ 已完成 | 2026-06-13 |
| CAS SQLite 分析 | ✅ 已完成 | 2026-06-13 |

---

**最后更新**: 2026-06-13  
**维护者**: Xet Server Team
