# 使用与运维 geometry_pyramid

[English](usage.md) · [README](../README.zh-CN.md)

本文面向负责构建、提供、刷新和升级 MVT 金字塔的数据库维护者。示例需在已安装
PostGIS 和 `geometry_pyramid` 的数据库中运行。从源码安装请先阅读
[README](../README.zh-CN.md#生成第一张瓦片)。

## 准备源表

每个数据库启用一次扩展：

```sql
CREATE EXTENSION IF NOT EXISTS postgis;
CREATE EXTENSION IF NOT EXISTS geometry_pyramid;
SELECT geometry_pyramid_version();
```

使用具有 geometry 列和稳定、唯一要素 ID 的普通表。`sourceSRS` 必须与 geometry
的 SRID 一致，默认值为 4326。geometry GiST 索引有助于构建、区域刷新和实时
瓦片读取。例如：

```sql
CREATE TABLE buildings (
    gid bigint PRIMARY KEY,
    name text,
    geom geometry(Polygon, 4326)
);
-- 先导入数据，再构建金字塔。
CREATE INDEX buildings_geom_gix ON buildings USING gist (geom);
```

源表、列和金字塔名称只接受最多 45 个字符的 ASCII 裸标识符；暂不支持带 schema
的源表名。
其他源投影可考虑在 `ST_Transform(geom, 3857)` 上建立表达式 GiST 索引，以便
源表查询与实时切片使用。

## 构建并验证

```sql
SELECT ST_BuildPyramid(
    'buildings', 'geom', 'gid',
    '{"name":"buildings","sourceSRS":4326,"maxZoom":14,"attributes":["name"]}'
);
SELECT ST_Pyramid('buildings');
SELECT * FROM ST_Pyramids();
```

构建成功时 `ST_BuildPyramid` 返回 `true`。`ST_Pyramid` 包含配置及逐 zoom
瓦片数、字节数、耗时和重试统计；`ST_Pyramids()` 列出已注册金字塔。首次构建
绑定源表和输出表的数据库对象，后续读取不依赖调用者的 `search_path`。

对同一名称和源表再次构建会执行全量重建。构建器在一个事务内替换正式表的瓦片行，
保留表的 OID、授权和依赖视图；事务失败会回滚瓦片变更。批量导入源数据后，应先
确认导入结果，再重建金字塔。

## 读取与提供瓦片

```sql
SELECT ST_Tile('buildings', 14, 13488, 6210);
SELECT octet_length(ST_Tile('buildings', 14, 13488, 6210));
```

`ST_Tile(name, z, x, y)` 返回 MVT `bytea`。空瓦片、未知金字塔、低于
`minZoom` 的层级，或合法 zoom 下越界的 x/y 返回 `NULL`。zoom 超出 0 至 24
或名称非法则报错。

在 `maxZoom` 内优先返回已物化瓦片。默认 `splitSize:0` 时，存储表未命中
表示空瓦片。设置 `splitSize > 0` 后，未命中可能是延迟构建的稀疏瓦片，每次
请求都会从当前源表实时切片。超过 `maxZoom` 默认也会实时切片；设置
`dynamicBeyondMaxZoom:false` 才会返回 `NULL`。扩展内部不缓存实时切片。

HTTP 服务可将 `GET /tiles/{name}/{z}/{x}/{y}.pbf` 映射为参数化查询
`SELECT ST_Tile($1, $2, $3, $4)`：有字节时返回 MVT，`NULL` 时返回空响应。
[本地演示](../demo/README.zh-CN.md)包含小型服务和 MapLibre 查看器；其中的单连接
同步服务仅用于验收。

## 源数据变化后刷新

根据**旧位置和新位置**受到的影响选择刷新方式：

| 变化 | 调用 | 覆盖范围 |
|---|---|---|
| 变化较多或范围不确定 | `ST_UpdatePyramid('buildings', '')` | 全量重建 |
| 现存要素仍能被条件选中 | `ST_UpdatePyramid('buildings', 'gid IN (1, 2)')` | 当前匹配行相交的瓦片 |
| 局部新增、删除或移动 | `ST_UpdatePyramidByExtent('buildings', xmin, ymin, xmax, ymax, srid)` | 指定区域相交的全部瓦片 |

移动要素时，范围应同时包含**旧位置与新位置**，否则执行全量重建。谓词刷新无法
找到已删除行的旧位置。范围坐标使用传入的 SRID，例如：

```sql
SELECT ST_UpdatePyramidByExtent(
    'buildings', 116.30, 39.90, 116.42, 39.99, 4326
);
```

区域刷新在事务内替换脏瓦片行。部分投影坐标系或跨日期变更线的范围会退化为本连接
全量重建，以保证覆盖完整。刷新后查看 `ST_Pyramid('buildings')`，并抽查受影响瓦片。

删除金字塔及其瓦片存储：

```sql
SELECT ST_DropPyramid('buildings');
```

请在删除绑定的源表**之前**删除金字塔。该操作会移除瓦片和统计；重建需要源数据。

## 配置参考

`ST_BuildPyramid` 的配置是 JSON 文档，`'{}'` 使用默认值。`maxLevel` 是
`maxZoom` 的别名，两者不能同时传入。

| Key | 默认值 | 作用或约束 |
|---|---:|---|
| `name` | 源表名 | 金字塔和瓦片表的标识。 |
| `sourceSRS` | `4326` | 源 geometry 的 SRID，应与实际列一致。 |
| `tileSRS` | `3857` | 仅接受 EPSG:3857。 |
| `minZoom`、`maxZoom` | `0`、`18` | 物化范围，包含两端；`maxZoom <= 24`。 |
| `tileSize`、`tileExtend` | `4096`、`8` | MVT extent（1 至 16384）和边缘 buffer（小于 extent）。 |
| `attributes` | `[]` | 写入 MVT feature 的非几何源字段。 |
| `dynamicBeyondMaxZoom` | `true` | 对超过 `maxZoom` 的合法 zoom 实时切片。 |
| `splitSize` | `0` | 低于要素数量阈值的瓦片改为实时生成；0 物化所有非空瓦片。 |
| `parallel` | `1` | 包括协调端的构建槽位；0 自动选择，最多 32。 |
| `preparedBuild` | `false` | 切片前预投影源几何。 |
| `featureMajorPoints` | `false` | 纯 POINT 稀疏路径；需要 `preparedBuild:true`、`parallel:1`、`tileSRS:3857`。 |
| `featureMajorMinZoom` | `13` | 点路径启用的最小 `maxZoom`；0 取消 zoom 门槛。 |
| `featureDrivenEnum` | `false` | 从要素包围盒枚举候选瓦片；需要 `preparedBuild:true` 和 `tileSRS:3857`。 |
| `simplifyPixels` | `1.0` | 几何简化容差；0 关闭。 |
| `minFeatureSizePixels` | `0.0` | 过滤过小的线和面；0 关闭。 |
| `dropRate` | `1.0` | 按 zoom 的点保留系数；1 不抽稀。 |
| `tileSizeBudgetBytes` | `0` | 超出未压缩 MVT 字节目标时加强抽稀并重试；0 关闭。不能与 `parallel > 1` 同用。 |
| `maxTileSizeAttempts` | `5` | 每层重试上限；字节预算是目标，不保证硬上限。 |
| `buildTimeoutMs` | `0` | 并行 worker/预处理等待上限，单位毫秒；0 不限时。 |
| `buildRules` | `[]` | 按 zoom 过滤、控制属性、采样或使用点网格。 |

可选的加速路径依赖数据形态。建议从默认值开始，在实际数据上测量。
[已有实测](../docs/user-data-build-performance.md)只是样例，不代表通用性能承诺。

### 使用 buildRules 控制各层细节

每条规则需要 `minZoom`、`maxZoom` 和至少一种操作：

| 操作 | 格式 | 行为 |
|---|---|---|
| 过滤 | `"filter":"importance >= 80"` | 针对源字段的 SQL 布尔条件；重叠条件按 AND 组合。 |
| 属性 | `"attrFields":["name"]` | 覆盖相应 zoom 输出的属性。 |
| 采样 | `"sample":{"rate":25,"key":["gid"]}` | 确定性保留百分比，`rate` 范围为 0 至 100；`key` 默认使用要素 ID。可用 `geometryTypes` 限定几何类型；重叠采样按 AND 组合。 |
| 点网格 | `"pointGrid":{"cellPixels":64,"keepField":"important"}` | 每格保留 ID 最小的点；若有标记点则保留标记点。`cellPixels` 范围是 1 至 `tileSize`。 |

重叠的 `attrFields` 规则必须一致。`keepField` 必须是源表布尔列；点网格不会
改变多点、线和面。

例如，粗层只保留重要建筑及较少属性：

```json
{
  "name": "buildings",
  "maxZoom": 14,
  "attributes": ["name", "importance"],
  "buildRules": [
    {"minZoom": 0, "maxZoom": 5,
     "filter": "importance >= 80", "attrFields": ["name"]}
  ]
}
```

`buildRules.filter` 和 `ST_UpdatePyramid(where_clause)` 是以调用者权限执行的
原始 SQL 条件，只能作为**受信任管理员输入**。`filter` 的分号检查无法让任意输入
变安全；不要将用户文本传入这些字段，也不要通过 `SECURITY DEFINER` 封装暴露。

## 从 0.1.0 升级

先在 PostgreSQL 服务器安装 0.1.1 扩展文件。0.1.0 只记录了裸表名，因此必须在
升级前核实**每个金字塔**实际对应的带 schema 源表及瓦片表。先备份数据库并安排
维护窗口。下面的关系名称仅作示例，必须替换为核实后的对象。

```sql
SELECT pyramid_name, source_table FROM public.geometry_pyramid_catalog;
-- 继续之前，在 pg_class 中核实每个源表和 _gp_tiles_<name> 表。
```

升级和全部绑定应在同一事务内完成：

```sql
BEGIN;
ALTER EXTENSION geometry_pyramid UPDATE TO '0.1.1';
UPDATE public.geometry_pyramid_catalog
SET source_rel = 'data.buildings'::regclass,
    storage_rel = 'tiles._gp_tiles_buildings'::regclass
WHERE pyramid_name = 'buildings';
-- 对每个已核实的金字塔重复 UPDATE。
DO $$ BEGIN
    IF EXISTS (SELECT FROM public.geometry_pyramid_catalog
               WHERE source_rel IS NULL OR storage_rel IS NULL) THEN
        RAISE EXCEPTION 'unbound pyramids remain';
    END IF;
END $$;
COMMIT;
```

出错时执行 `ROLLBACK`，修正关系映射后再试。完成后查看 `ST_Pyramids()`、
`ST_Pyramid(name)` 及一张已知瓦片。旧金字塔若使用过瓦片字节预算，应先全量
刷新，再依赖延迟瓦片的实时读取；刷新会记录每层最终使用的参数。

## 排障

| 症状 | 检查或处理 |
|---|---|
| `CREATE EXTENSION` 失败 | 检查 PostgreSQL 和 PostGIS 安装，以及扩展包与服务器主版本是否匹配。 |
| 构建拒绝名称 | 使用最多 45 个字符的 ASCII 裸标识符；源表名不能带 schema。 |
| 构建拒绝 `tileSRS` | 设为 3857；当前瓦片网格仅支持 Web Mercator。 |
| `ST_Tile` 返回 `NULL` | 检查名称、覆盖范围、`minZoom`、`maxZoom` 和 `dynamicBeyondMaxZoom`；空瓦片返回 `NULL`。0 至 24 之外的 zoom 会报错。 |
| 删除或移动后留有旧瓦片 | 对包含旧位置的范围刷新，或全量重建。 |
| 实时瓦片读取缓慢 | 检查 `splitSize` 和高于 `maxZoom` 的请求；增加服务端缓存或物化更多瓦片。 |
| 构建缓慢 | 检查几何索引、源数据范围、zoom 范围和逐层统计；在目标数据上测量 `preparedBuild`、按要素路径或 `buildRules`。 |
| 并行构建拒绝字节预算 | 使用 `parallel:1` 配合 `tileSizeBudgetBytes`，或关闭字节预算。 |
| 过滤条件失败 | 用同一角色对源表测试条件，不要传入不受信任的文本。 |

开发检查和基准脚本见[运维、测试与基准](operations/testing-and-benchmarks.zh-CN.md)。
