# 本地瓦片服务与前端交互查看器操作说明

<p align="center">
  <a href="README.md">English</a> •
  <a href="README.zh-CN.md">简体中文</a> •
  <a href="../README.zh-CN.md">返回项目主页</a>
</p>

本指南提供在本地快速启动轻量级矢量瓦片 HTTP 服务并使用 MapLibre GL 交互式地图查看器的完整操作说明。用于端到端验证 `ST_BuildPyramid` 生成的物化瓦片、`splitSize` 稀疏瓦片切片以及大于 `maxZoom` 的实时动态切片行为。

---

## 📋 架构说明

- **后端服务**：基于 Python 3 与 `psycopg` 构建的轻量 HTTP 瓦片服务器（单连接同步模式，仅作为本地功能与视觉验收工具，非高并发生产服务器）。
- **前端查看**：基于本地内嵌的 MapLibre GL JS 与 CSS，不依赖外网 CDN；底图提供可选的 OpenStreetMap 开关（默认离线纯瓦片模式）。
- **数据注入**：`demo/seed.sql` 自动在数据库中生成随机测试点集 `demo_src`（集中在北京及周边区域），并构建名为 `demo` 的金字塔。

---

## 🚀 完整操作流程

### 第一步：准备并启动 PostgreSQL 实例

在项目根目录下，启动 pgrx 管理的 PostgreSQL 实例（以 PG17 为例）：

```bash
cargo pgrx run pg17
```

在自动打开的 `psql` 交互终端中，创建一个专用的演示数据库（若尚未创建）：

```sql
CREATE DATABASE gp_demo;
\connect gp_demo
\conninfo
\q
```

记下 `\conninfo` 打印的连接端口（例如 `28817` 或系统当前分配的端口）。

---

### 第二步：导入演示数据并构建金字塔

在新的终端窗口中，设置 `DATABASE_URL` 并执行 `seed.sql` 脚本：

```bash
# 将 28817 替换为您实际观察到的 PostgreSQL 端口
export DATABASE_URL="host=localhost port=28817 dbname=gp_demo"

# 执行初始化建表与金字塔构建
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -f demo/seed.sql
```

**预期输出**：
脚本最后一步会查询 0 级瓦片字节大小，应输出类似如下非零字节数：
```text
 z0 bytes 
----------
      142
(1 row)
```

> **提示**：`demo/seed.sql` 具备幂等性，重复执行时会自动清理并重建旧的 `demo` 金字塔和 `demo_src` 源表。

---

### 第三步：安装 Python 依赖并启动服务

建议在独立的虚拟环境中运行：

```bash
# 1. 创建并激活虚拟环境
python3 -m venv /tmp/gp-demo-venv
source /tmp/gp-demo-venv/bin/activate

# 2. 安装 PostgreSQL 驱动
pip install 'psycopg[binary]'

# 3. 启动瓦片服务（默认监听 127.0.0.1:8989）
python demo/server.py
```

如需更换监听端口，可在启动前设置 `PORT` 环境变量，例如 `export PORT=9000`。

---

### 第四步：浏览器查看与功能验收

在浏览器中打开：[http://127.0.0.1:8989](http://127.0.0.1:8989)

#### 验收要点：
1. **基础展示**：地图中心对准东经 116.49°、北纬 40° 附近，页面渲染出红色密集散点。
2. **物化瓦片验证（0 ~ 8 级）**：缩放地图至 0~8 级，网络面板显示瓦片请求均由数据库物化表直接毫秒级返回。
3. **实时动态切片验证（> 8 级）**：继续放大到 9 级以上，触发超出 `maxZoom: 8` 的实时切片机制，瓦片仍能平滑按需实时渲染。
4. **稀疏延迟切片验证（`splitSize`）**：平移至离群稀疏点区域，验证要素数低于阈值瓦片的实时切片逻辑。

---

### 第五步：自动化接口验证

在另一个终端使用 curl 或执行自带测试脚本：

```bash
# 验证 HTML 入口
curl -sS -o /dev/null -w "Status: %{http_code}\n" http://127.0.0.1:8989/

# 验证瓦片接口 (z=0, x=0, y=0)
curl -sS -o /dev/null -w "Status: %{http_code}\n" http://127.0.0.1:8989/tiles/demo/0/0/0.pbf

# 运行自带单元与集成测试脚本
python demo/test_server.py
```

**HTTP 状态码约定**：
- `200 OK`：瓦片存在且包含要素（返回 `application/x-protobuf`）。
- `204 No Content`：空瓦片（该区域无要素）。
- `404 Not Found`：非法 URL 路由或参数格式错误。
- `500 Internal Server Error`：数据库查询异常或连接中断。

---

## 🧹 清理演示数据

若仅需清空演示数据并释放存储，可在 `gp_demo` 数据库中执行：

```sql
SELECT ST_DropPyramid('demo');
DROP TABLE demo_src;
```
