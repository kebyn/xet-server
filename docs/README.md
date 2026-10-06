# 文档索引

本索引列出 Xet Server 项目的所有文档。

## 核心文档

### 项目文档
- **[README.md](../README.md)** - 项目概述、快速开始、使用示例（英文）
- **[README.zh-CN.md](../README.zh-CN.md)** - 项目概述、快速开始、使用示例（中文）
- **[HF_XET_INTEGRATION_GUIDE.md](../HF_XET_INTEGRATION_GUIDE.md)** - HuggingFace 集成指南

### 用户指南
- **[配置指南](configuration.md)** - 完整的配置选项说明
- **[架构文档](architecture.md)** - 系统架构和数据流说明

## API 文档

### API 参考
- **[CAS API](api/cas-api.md)** - CAS 服务器 API 详细参考
- **[Hub API](api/hub-api.md)** - Hub API 详细参考
- **[认证文档](api/authentication.md)** - Ed25519 JWT 认证机制

## 文档结构

```
/data/
├── README.md                              # 项目主文档（英文）
├── README.zh-CN.md                        # 项目主文档（中文）
├── LICENSE                                # MIT 许可证
├── HF_XET_INTEGRATION_GUIDE.md           # HuggingFace 集成指南
│
├── docs/
│   ├── README.md                         # 本文档索引
│   ├── configuration.md                  # 配置指南
│   ├── architecture.md                   # 架构文档
│   └── api/                              # API 文档
│       ├── cas-api.md                    # CAS API 参考
│       ├── hub-api.md                    # Hub API 参考
│       └── authentication.md             # 认证文档
```

## 快速导航

### 新用户
1. 阅读 [README.md](../README.md)（英文）或 [README.zh-CN.md](../README.zh-CN.md)（中文）了解项目概述
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
2. 遵循代码规范和测试要求

## 文档状态

| 文档 | 状态 | 最后更新 |
|------|------|----------|
| README.md | ✅ 完成 | 2026-10-06 |
| HF_XET_INTEGRATION_GUIDE.md | ✅ 完成 | 2026-07-28 |
| 配置指南 | ✅ 完成 | 2026-07-31 |
| 架构文档 | ✅ 完成 | 2026-07-31 |
| CAS API 文档 | ✅ 完成 | 2026-10-06 |
| Hub API 文档 | ✅ 完成 | 2026-07-31 |
| 认证文档 | ✅ 完成 | 2026-07-28 |

---

**最后更新**: 2026-10-06  
**维护者**: Xet Server Team
