#!/usr/bin/env python3
"""Benchmark existing gpbench fixtures using installed psycopg.

PGHOST=... PGPORT=28817 PGDATABASE=postgres python3 bench/mvt_optimization.py before.json
Optional second argument: absolute immutable library path (without .dylib),
loaded through an isolated C wrapper instead of the globally installed build.
Add --small-parallel as third argument to run only prepared points_3857 with
parallel=4, testing the small-extent coordinator path (all zoom rectangles <=64).
Alternatively --sparse runs synthetic sparse circles/points at z12–14.
Run before/after against the same snapshot, without concurrent workloads.
Only creates/drops its own gpopt_measure schema; source fixtures are read-only.
"""
import hashlib
import json
import statistics
import sys
import time
import psycopg


def varint(data, pos):
    value = shift = 0
    while True:
        byte = data[pos]
        pos += 1
        value |= (byte & 127) << shift
        if byte < 128:
            return value, pos
        shift += 7


def fields(data):
    pos = 0
    while pos < len(data):
        tag, pos = varint(data, pos)
        wire = tag & 7
        if wire == 0:
            value, pos = varint(data, pos)
        elif wire == 2:
            size, pos = varint(data, pos)
            value, pos = data[pos:pos + size], pos + size
        elif wire in (1, 5):
            size = 8 if wire == 1 else 4
            value, pos = data[pos:pos + size], pos + size
        else:
            raise ValueError(f"Unexpected protobuf wire type {wire}")
        yield tag >> 3, value


def packed(data):
    values, pos = [], 0
    while pos < len(data):
        value, pos = varint(data, pos)
        values.append(value)
    return values


def canonical_tile(data):
    """Resolve dictionaries; ignore feature/key/value ordering, retain duplicates."""
    layers, count = [], 0
    for field, layer in fields(data):
        if field != 3:
            continue
        entries = list(fields(layer))
        keys = [v.hex() for k, v in entries if k == 3]
        vals = [v.hex() for k, v in entries if k == 4]
        features = []
        for key, feature in entries:
            if key != 2:
                continue
            attrs, identity, kind, geometry = [], 0, 0, []
            for k, v in fields(feature):
                if k == 1:
                    identity = v
                elif k == 2:
                    tags = packed(v)
                    attrs.extend((keys[tags[i]], vals[tags[i + 1]]) for i in range(0, len(tags), 2))
                elif k == 3:
                    kind = v
                elif k == 4:
                    geometry.extend(packed(v))
            features.append(json.dumps([identity, sorted(attrs), kind, geometry], separators=(",", ":")))
        count += len(features)
        meta = [(k, v.hex() if isinstance(v, bytes) else v) for k, v in entries if k in (1, 5, 15)]
        layers.append([sorted(meta), sorted(features)])
    return json.dumps(sorted(layers), separators=(",", ":")).encode(), count


assert varint(b"\xac\x02", 0) == (300, 2)
assert list(fields(b"\x08\x01\x12\x01a")) == [(1, 1), (2, b"a")]
assert canonical_tile(b"") == (b"[]", 0)
# Same feature with its tag dictionary remapped (and unused dictionary entries).
assert canonical_tile(bytes.fromhex("1a1d0a01782880207802120b08071202000018012201001a016b22030a0176")) == canonical_tile(bytes.fromhex("1a250a01782880207802120b08071202010118012201001a016a1a016b22030a017522030a0176"))

conn = psycopg.connect(autocommit=True)
cur = conn.cursor()
cur.execute("SET search_path=gpopt_measure,gpbench,public")
cur.execute("SET jit=off")
cur.execute("SET max_parallel_workers_per_gather=0")
cur.execute("SET statement_timeout=300000")
cur.execute("SELECT count(*) FROM geometry_pyramid_catalog WHERE pyramid_name='gpopt_measurement'")
if cur.fetchone()[0]:
    raise RuntimeError("Benchmark pyramid name already exists; refusing to overwrite")
cur.execute("CREATE SCHEMA gpopt_measure")
report = {"method": "one warmup + three timed full ST_BuildPyramid calls; client wall includes commit; drop excluded", "cases": []}
sparse = "--sparse" in sys.argv[3:]
try:
    if len(sys.argv) > 2:
        from psycopg import sql
        cur.execute(sql.SQL("CREATE FUNCTION gpopt_measure.st_buildpyramid(text,text,text,text) RETURNS boolean AS {}, 'st_build_pyramid_wrapper' LANGUAGE c STRICT VOLATILE").format(sql.Literal(sys.argv[2])))
        report["build_library"] = sys.argv[2]
    cur.execute("SELECT version(),postgis_full_version(),current_setting('work_mem'),current_setting('jit'),current_setting('max_parallel_workers_per_gather')")
    report["environment"] = cur.fetchone()
    cur.execute("CREATE TABLE gpopt_measure.lucc_sample AS SELECT * FROM gpbench.lucc_3857 ORDER BY ogc_fid LIMIT 250")
    cur.execute("CREATE INDEX ON gpopt_measure.lucc_sample USING gist(geom)")
    cur.execute("ANALYZE gpopt_measure.lucc_sample")
    cases = [("points_3857",3857,10,["id"]),("big_line_3857",3857,10,[]),("building_3857",3857,10,[]),("lucc_sample",3857,6,[]),("points_4326",4326,10,["id"])]
    if sparse:
        cur.execute("CREATE TABLE gpopt_measure.sparse_circles AS SELECT i AS ogc_fid, ST_Buffer(ST_Centroid(ST_TileEnvelope(14,6000+(i%40)*8,6000+(i/40)*8)),20,128) AS geom FROM generate_series(0,999) i")
        cur.execute("CREATE TABLE gpopt_measure.sparse_points AS SELECT i AS ogc_fid, ST_Centroid(ST_TileEnvelope(14,6000+((i%1000)%40)*8,6000+((i%1000)/40)*8)) AS geom FROM generate_series(0,9999) i")
        cur.execute("CREATE INDEX ON gpopt_measure.sparse_circles USING gist(geom)")
        cur.execute("CREATE INDEX ON gpopt_measure.sparse_points USING gist(geom)")
        cur.execute("ANALYZE gpopt_measure.sparse_circles")
        cur.execute("ANALYZE gpopt_measure.sparse_points")
        cases = [("sparse_circles",3857,14,[]),("sparse_points",3857,14,[])]
    for source, srid, zoom, attrs in cases:
        cur.execute(f"SELECT count(*),sum(ST_NPoints(geom)) FROM {source}")
        rows, vertices = cur.fetchone()
        for prepared in ((True,) if sparse else (False, True)):
            name = "gpopt_measurement"
            config = dict(name=name,sourceSRS=srid,tileSRS=3857,minZoom=0,maxZoom=zoom,preparedBuild=prepared,featureMajorPoints=False,featureDrivenEnum=False,attributes=attrs)
            if sparse:
                config.update(minZoom=12,tileExtend=8,simplifyPixels=1,featureMajorMinZoom=12,featureMajorPoints=source=="sparse_points",featureDrivenEnum=source=="sparse_circles")
            result = dict(source=source,source_rows=rows,source_vertices=vertices,config=config,runs_ms=[])
            for run in range(4):
                cur.execute("SELECT ST_DropPyramid(%s)",(name,))
                start = time.perf_counter()
                cur.execute("SELECT ST_BuildPyramid(%s,'geom','ogc_fid',%s)",(source,json.dumps(config)))
                result["runs_ms"].append(round((time.perf_counter()-start)*1000,3))
                print(source,prepared,run,result["runs_ms"][-1],flush=True)
            cur.execute("SELECT z,tile_count,total_bytes,max_tile_bytes,build_ms,attempts FROM geometry_pyramid_stats WHERE pyramid_name=%s ORDER BY z",(name,))
            result["zoom_stats"] = cur.fetchall()
            digest, features = hashlib.sha256(), 0
            cur.execute("SELECT z,x,y,mvt FROM gpopt_measure._gp_tiles_gpopt_measurement ORDER BY z,x,y")
            for z,x,y,mvt in cur:
                payload, count = canonical_tile(bytes(mvt))
                digest.update(f"{z}/{x}/{y}:".encode()+hashlib.sha256(payload).digest())
                features += count
            result.update(median_ms=statistics.median(result["runs_ms"][1:]),semantic_sha256=digest.hexdigest(),feature_count=features)
            report["cases"].append(result)
            with open(sys.argv[1],"w") as output:
                json.dump(report,output,indent=2)
            cur.execute("SELECT ST_DropPyramid(%s)",(name,))
finally:
    cur.execute("SELECT ST_DropPyramid('gpopt_measurement')")
    cur.execute("DROP TABLE IF EXISTS gpopt_measure.lucc_sample")
    if sparse:
        cur.execute("DROP TABLE IF EXISTS gpopt_measure.sparse_circles,gpopt_measure.sparse_points")
    if len(sys.argv) > 2:
        cur.execute("DROP FUNCTION IF EXISTS gpopt_measure.st_buildpyramid(text,text,text,text)")
    cur.execute("DROP SCHEMA gpopt_measure")
    conn.close()
