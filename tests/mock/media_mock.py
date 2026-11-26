"""Mock of the image sources: Openverse search, Wikimedia Commons API, and image bytes.

Replies follow the real APIs' field names and shapes (captured from api.openverse.org and
commons.wikimedia.org in November 2026; see crates/imagery/tests/fixtures/). Every image URL in a
reply points back at this mock, under /img/, and /img/ serves real, valid PNG or JPEG bytes
(generated with the standard library) at the size the URL asks for:

    /img/ov/<id>-<W>x<H>.jpg?w=960          an Openverse stock image, served 960 px wide
    /img/wikipedia/commons/a/ab/<Name>-<W>x<H>.jpg              a Commons original
    /img/wikipedia/commons/thumb/a/ab/<Name>/<w>px-<Name>       a Commons thumbnail; like the
                                            real thumbnailer, only standard widths are served
                                            (others answer 400)
    /openverse/v1/images/<id>/thumb/        Openverse's own small thumbnail (600 px)

Search behaviour, deterministic and driven by the query text:

    "nothing"   no results from either source (exercises "no suitable image")
    "sparse"    one usable result per search (exercises the second search wave)

Each search also returns realistic junk the client must filter or survive: a non-commercial
licence, a portrait, a too-small image, a preview that is an HTML error page served as
image/jpeg, a preview that 404s, a GFDL-only file, a TIFF scan.

CHAT_STAGES["curate"] answers the imagery crate's vision call: it checks every image is an inline
base64 data URL whose bytes are a real image, then picks photo 2 (or 1 if there is only one) with
a valid JSON verdict. A theme whose prompt contains "reject" gets "none is good enough".
"""

import base64
import hashlib
import re
import struct
import threading
import zlib

from mocklib import Response, error_response, json_response, not_found

LOCK = threading.Lock()
CACHE = {}  # (kind, width, height, seed) -> bytes
STATE = {"openverse": 0, "commons": 0, "images": 0, "curate": 0}

# Widths the Wikimedia thumbnailer serves.
STANDARD_WIDTHS = {20, 40, 60, 120, 250, 330, 500, 960, 1280, 1920, 3840}
# The mock refuses to generate anything bigger than this (the client never needs it).
MAX_SIDE = 4000

OPENVERSE_LICENSES = {"by", "by-nc", "by-nc-nd", "by-nc-sa", "by-nd", "by-sa", "cc0", "nc-sampling+", "pdm", "sampling+"}


def reset():
    with LOCK:
        for k in STATE:
            STATE[k] = 0


def _h(text, n=12):
    return hashlib.sha256(text.encode("utf-8")).hexdigest()[:n]


def _uuid(text):
    h = hashlib.sha256(text.encode("utf-8")).hexdigest()
    return f"{h[:8]}-{h[8:12]}-{h[12:16]}-{h[16:20]}-{h[20:32]}"


# ---------------------------------------------------------------------------------------------
# Image bytes (standard library only)
# ---------------------------------------------------------------------------------------------


def _png_chunk(kind, data):
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)


def png_bytes(width, height, seed):
    """A valid 8-bit RGB PNG: a vertical gradient between two colours derived from `seed`."""
    a = ((seed >> 16) & 0xFF, (seed >> 8) & 0xFF, seed & 0xFF)
    b = (255 - a[0], (a[1] + 90) % 256, (a[2] + 160) % 256)
    rows = []
    for y in range(height):
        t = y / max(1, height - 1)
        px = bytes(int(a[i] + (b[i] - a[i]) * t) for i in range(3))
        rows.append(b"\x00" + px * width)
    raw = b"".join(rows)
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + _png_chunk(b"IHDR", ihdr) + _png_chunk(b"IDAT", zlib.compress(raw, 6)) + _png_chunk(b"IEND", b"")


def jpeg_bytes(width, height):
    """A valid baseline greyscale JPEG of a uniform mid-grey: every 8x8 block codes a DC difference
    of 0 and an end-of-block, one bit each with one-code Huffman tables."""
    out = bytearray(b"\xff\xd8")
    out += b"\xff\xe0\x00\x10JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00"
    out += b"\xff\xdb\x00\x43\x00" + bytes([1] * 64)
    out += b"\xff\xc0\x00\x0b\x08" + struct.pack(">HH", height, width) + b"\x01\x01\x11\x00"
    one_code = bytes([1] + [0] * 15) + b"\x00"
    out += b"\xff\xc4\x00\x14\x00" + one_code  # DC table 0
    out += b"\xff\xc4\x00\x14\x10" + one_code  # AC table 0
    out += b"\xff\xda\x00\x08\x01\x01\x00\x00\x3f\x00"
    blocks = ((width + 7) // 8) * ((height + 7) // 8)
    bits = 2 * blocks
    full, rest = divmod(bits, 8)
    out += bytes(full)
    if rest:
        out.append(0xFF >> rest)  # remaining bits are zero codes, padded with ones
    out += b"\xff\xd9"
    return bytes(out)


def image_bytes(kind, width, height, seed):
    key = (kind, width, height, seed if kind == "png" else 0)
    with LOCK:
        if key in CACHE:
            return CACHE[key]
    data = png_bytes(width, height, seed) if kind == "png" else jpeg_bytes(width, height)
    with LOCK:
        if len(CACHE) > 64:
            CACHE.clear()
        CACHE[key] = data
    return data


DIMS = re.compile(r"(\d{2,5})x(\d{2,5})")


def image(req):
    """GET /img/...: real image bytes at the size the URL asks for (see the module docstring)."""
    with LOCK:
        STATE["images"] += 1
    name = req.path.rsplit("/", 1)[-1]
    if "missing" in name:
        return Response(404, b"<html><body>404 Not Found</body></html>", "text/html; charset=utf-8")
    if "broken" in name:
        # A server that lies: an HTML error page labelled as a JPEG.
        return Response(200, b"<!DOCTYPE html><html><body><h1>Something went wrong</h1></body></html>", "image/jpeg")
    found = DIMS.findall(req.path)
    if not found:
        return not_found(req.path)
    ow, oh = (int(v) for v in found[-1])
    px = re.search(r"(?:^|-)(\d+)px-", name)
    if px:
        tw = int(px.group(1))
        if tw not in STANDARD_WIDTHS or tw > ow:
            return Response(400, b"<html><body>Error generating thumbnail: unsupported width</body></html>", "text/html; charset=utf-8")
    elif "w" in req.query:
        try:
            tw = int(req.query["w"])
        except ValueError:
            return error_response(400, "bad width")
    else:
        tw = ow
    tw = max(1, min(tw, ow))
    th = max(1, round(tw * oh / ow))
    if tw > MAX_SIDE or th > MAX_SIDE:
        return error_response(403, f"mock: {tw}x{th} is larger than the mock serves")
    kind = "png" if name.lower().split("?")[0].endswith(".png") else "jpeg"
    seed = int(_h(req.path, 6), 16)
    return Response(200, image_bytes(kind, tw, th, seed), "image/png" if kind == "png" else "image/jpeg", {"Cache-Control": "max-age=3600"})


# ---------------------------------------------------------------------------------------------
# Openverse: GET /openverse/v1/images/?q=...
# ---------------------------------------------------------------------------------------------

# source, licence, version, width, height, mature, category, filetype, special
OPENVERSE_ROWS = [
    ("stocksnap", "cc0", "1.0", 4800, 3200, False, "photograph", "jpg", ""),
    ("rawpixel", "cc0", "1.0", 5760, 3840, False, "photograph", "jpg", ""),
    ("wikimedia", "by-sa", "4.0", 5000, 3333, False, None, "jpg", "commons"),
    ("flickr", "by", "2.0", 4000, 2250, False, "photograph", "jpg", ""),
    ("stocksnap", "cc0", "1.0", 4000, 2667, False, "photograph", "jpg", "broken"),
    ("flickr", "by-sa", "2.0", 4000, 2667, False, "photograph", "jpg", "missing"),
    ("stocksnap", "cc0", "1.0", 4000, 2667, False, "photograph", "jpg", "liar"),
    ("flickr", "by-nc", "2.0", 4000, 2667, False, "photograph", "jpg", ""),
    ("rawpixel", "cc0", "1.0", 3000, 4500, False, "photograph", "jpg", ""),
    ("rawpixel", "pdm", "1.0", 1200, 800, False, "photograph", "jpg", ""),
    ("flickr", "by", "2.0", 4000, 2667, True, "photograph", "jpg", ""),
    ("smithsonian_national_museum", "cc0", "1.0", 3000, 2000, False, "digitized_artwork", "jpg", ""),
]
SUFFIXES = ["at golden hour", "close up", "in soft light", "from above", "still life", "details", "wide view", "evening", "morning", "texture", "scene", "study"]


def _openverse_result(host, query, i, row):
    source, lic, ver, w, h, mature, category, filetype, special = row
    rid = _uuid(f"ov|{query}|{i}")
    slug = re.sub(r"[^a-z0-9]+", "-", query.lower()).strip("-") or "photo"
    title = f"{query.title()} {SUFFIXES[i % len(SUFFIXES)]}"
    if special == "commons":
        name = f"{slug.title().replace('-', '_')}_{rid[:6]}-{w}x{h}.jpg"
        a = rid[0]
        url = f"http://{host}/img/wikipedia/commons/{a}/{a}{rid[1]}/{name}"
        landing = f"https://commons.wikimedia.org/wiki/File:{name}"
        title = f"File:{name}"
    elif special == "liar":
        # The metadata says landscape; the image itself is portrait.
        url = f"http://{host}/img/ov/{rid}-liar-640x960.jpg?w=640"
        landing = f"https://{source}.example/photo/{rid}"
    else:
        tag = f"-{special}" if special else ""
        served = 1024 if source in ("rawpixel", "flickr") else 960
        url = f"http://{host}/img/ov/{rid}{tag}-{w}x{h}.jpg?w={served}"
        landing = f"https://{source}.example/photo/{rid}"
    thumb_id = f"{rid}-{special}" if special in ("missing", "liar") else rid
    return {
        "id": rid,
        "title": title,
        "indexed_on": "2024-03-01T12:00:00.000000Z",
        "foreign_landing_url": landing,
        "url": url,
        "creator": f"Mock Photographer {i + 1}",
        "creator_url": f"https://{source}.example/people/{i + 1}",
        "license": lic,
        "license_version": ver,
        "license_url": f"https://creativecommons.org/licenses/{lic}/{ver}/",
        "provider": source,
        "source": source,
        "category": category,
        "filesize": 250000 + i * 1000,
        "filetype": filetype,
        "tags": [{"name": t, "accuracy": None, "unstable__provider": source} for t in query.lower().split()[:6]],
        "attribution": f"\"{title}\" by Mock Photographer {i + 1} is licensed under CC {lic.upper()} {ver}.",
        "fields_matched": ["title", "tags.name"],
        "mature": mature,
        "height": h,
        "width": w,
        "thumbnail": f"http://{host}/openverse/v1/images/{thumb_id}/thumb/",
        "detail_url": f"http://{host}/openverse/v1/images/{rid}/",
        "related_url": f"http://{host}/openverse/v1/images/{rid}/related/",
        "unstable__sensitivity": ["user_reported_sensitive"] if mature else [],
    }


def openverse(req):
    host = req.headers.get("host", "127.0.0.1")
    m = re.match(r"^/openverse/v1/images/([^/]+)/thumb/?$", req.path)
    if m:
        if "missing" in m.group(1):
            return not_found(req.path)
        if "liar" in m.group(1):
            return Response(200, image_bytes("jpeg", 400, 600, 0), "image/jpeg")
        return Response(200, image_bytes("jpeg", 600, 400, 0), "image/jpeg")
    if req.path.rstrip("/") != "/openverse/v1/images":
        return not_found(req.path)
    with LOCK:
        STATE["openverse"] += 1
    q = (req.query.get("q") or "").strip()
    try:
        page_size = int(req.query.get("page_size") or 20)
    except ValueError:
        return json_response({"detail": {"page_size": ["A valid integer is required."]}}, 400)
    if page_size > 20:
        return json_response({"detail": "Authentication credentials were not provided."}, 401)
    licenses = [x.strip() for x in (req.query.get("license") or "").split(",") if x.strip()]
    unknown = [x for x in licenses if x not in OPENVERSE_LICENSES]
    if unknown:
        return json_response({"detail": {"license": [f"Invalid licenses: {', '.join(unknown)}"]}}, 400)
    rows = [] if "nothing" in q.lower() or not q else list(enumerate(OPENVERSE_ROWS))
    if "sparse" in q.lower():
        rows = rows[:1]
    if licenses:
        rows = [(i, r) for i, r in rows if r[1] in licenses]
    if req.query.get("mature", "false").lower() != "true":
        rows = [(i, r) for i, r in rows if not r[5]]
    results = [_openverse_result(host, q, i, r) for i, r in rows][:page_size]
    count = len(results)
    return json_response({"result_count": count, "page_count": 1 if count else 0, "page_size": page_size, "page": 1, "results": results})


# ---------------------------------------------------------------------------------------------
# Wikimedia Commons: GET /commons/w/api.php?action=query&generator=search&...
# ---------------------------------------------------------------------------------------------

# width, height, mime, LicenseShortName, License, quality, artist kind
COMMONS_ROWS = [
    (6000, 4000, "image/jpeg", "CC BY-SA 4.0", "cc-by-sa-4.0", True, "link"),
    (5472, 3648, "image/jpeg", "CC0", "cc0", True, "plain"),
    (4928, 3264, "image/png", "Public domain", "pd", True, "list"),
    (5000, 2813, "image/jpeg", "CC BY 2.0", "cc-by-2.0", True, "link"),
    (4000, 6000, "image/jpeg", "CC BY-SA 4.0", "cc-by-sa-4.0", False, "link"),
    (4500, 3000, "image/jpeg", "GFDL", "gfdl", False, "link"),
    (5200, 3467, "image/tiff", "Public domain", "pd", False, "plain"),
    (3600, 2400, "image/jpeg", "CC BY-SA 3.0", "cc-by-sa-3.0", False, "link"),
]


def _artist(kind, i):
    if kind == "plain":
        return f"Mock Artist {i + 1}"
    if kind == "list":
        return ('<ul><li><b>Original: </b> <a href="//commons.wikimedia.org/wiki/User:Mock_Original" title="User:Mock Original">Mock Original</a></li>\n'
                '<li><b>Derivative work: </b> <a href="//commons.wikimedia.org/wiki/User:Mock_Editor" title="User:Mock Editor">Mock Editor</a></li></ul>')
    return (f'<bdi><a href="https://www.wikidata.org/wiki/Q{1000 + i}" class="extiw" title="d:Q{1000 + i}">'
            f'<span title="photographer">Mock Artist {i + 1}</span></a></bdi>')


def _thumb(host, a, ab, name, width):
    # Thumbnails of TIFF scans are JPEGs named like the real thumbnailer names them.
    last = f"lossy-page1-{width}px-{name}.jpg" if name.lower().endswith((".tif", ".tiff")) else f"{width}px-{name}"
    return (f"http://{host}/img/wikipedia/commons/thumb/{a}/{ab}/{name}/{last}"
            "?utm_source=commons.wikimedia.org&utm_campaign=imageinfo&utm_content=thumbnail")


def _commons_page(host, query, i, row, thumb_width):
    w, h, mime, short, code, _quality, artist = row
    ext = {"image/jpeg": "jpg", "image/png": "png", "image/tiff": "tif"}[mime]
    words = re.sub(r"[^A-Za-z0-9 ]+", " ", query).split() or ["Photo"]
    name = f"{'_'.join(x.capitalize() for x in words)}_{i + 1:02d}-{w}x{h}.{ext}"
    digest = hashlib.md5(name.encode("utf-8")).hexdigest()
    a, ab = digest[0], digest[:2]
    original = f"http://{host}/img/wikipedia/commons/{a}/{ab}/{name}?utm_source=commons.wikimedia.org&utm_campaign=imageinfo&utm_content=original"
    info = {
        "size": w * h // 3,
        "width": w,
        "height": h,
        "url": original,
        "descriptionurl": f"https://commons.wikimedia.org/wiki/File:{name}",
        "descriptionshorturl": f"https://commons.wikimedia.org/wiki/Special:Redirect/page/{70000000 + i}",
        "extmetadata": {
            "ObjectName": {"value": name.rsplit(".", 1)[0].replace("_", " ").rsplit("-", 1)[0], "source": "mediawiki-metadata"},
            "Artist": {"value": _artist(artist, i), "source": "commons-desc-page"},
            "LicenseShortName": {"value": short, "source": "commons-desc-page", "hidden": ""},
            "License": {"value": code, "source": "commons-templates", "hidden": ""},
        },
        "mime": mime,
    }
    if thumb_width:
        tw = min(thumb_width, w)
        info["thumburl"] = _thumb(host, a, ab, name, tw) if tw < w else original
        info["thumbwidth"] = tw
        info["thumbheight"] = round(tw * h / w)
        if 2 * tw < w and 2 * tw in STANDARD_WIDTHS:
            info["responsiveUrls"] = {"2": _thumb(host, a, ab, name, 2 * tw)}
    return {"pageid": 70000000 + i, "ns": 6, "title": f"File:{name.replace('_', ' ')}", "index": i + 1, "imagerepository": "local", "imageinfo": [info]}


def commons(req):
    if req.path != "/commons/w/api.php":
        return not_found(req.path)
    with LOCK:
        STATE["commons"] += 1
    q = req.query
    if q.get("action") != "query":
        return json_response({"error": {"code": "badvalue", "info": "Unrecognized value for parameter \"action\"."}})
    if q.get("generator") != "search":
        return json_response({"batchcomplete": True} if q.get("formatversion") == "2" else {"batchcomplete": ""})
    search = q.get("gsrsearch") or ""
    quality = "incategory:Quality_images" in search
    text = " ".join(t for t in search.split() if ":" not in t)
    try:
        limit = max(1, min(50, int(q.get("gsrlimit") or 10)))
        thumb_width = int(q.get("iiurlwidth") or 0)
    except ValueError:
        return json_response({"error": {"code": "badinteger", "info": "Invalid value for an integer parameter."}})
    if "nothing" in text.lower() or not text:
        return json_response({"batchcomplete": True} if q.get("formatversion") == "2" else {"batchcomplete": ""})
    host = req.headers.get("host", "127.0.0.1")
    rows = [(i, r) for i, r in enumerate(COMMONS_ROWS) if r[5] or not quality]
    if "sparse" in text.lower():
        rows = rows[:1]
    rows = rows[:limit]
    pages = [_commons_page(host, text, i, r, thumb_width) for i, r in rows]
    # Like the real API, pages are not listed in search order (that is in "index").
    pages = pages[1::2] + pages[0::2]
    body = {"batchcomplete": True if q.get("formatversion") == "2" else ""}
    if len(rows) == limit:
        body["continue"] = {"gsroffset": limit, "continue": "gsroffset||"}
    body["query"] = {"pages": pages if q.get("formatversion") == "2" else {str(p["pageid"]): p for p in pages}}
    return json_response(body)


# ---------------------------------------------------------------------------------------------
# The vision stage, for deepseek_mock (looked up through CHAT_STAGES)
# ---------------------------------------------------------------------------------------------

MAGIC = {"image/png": (b"\x89PNG\r\n\x1a\n",), "image/jpeg": (b"\xff\xd8\xff",), "image/webp": (b"RIFF",), "image/gif": (b"GIF87a", b"GIF89a")}


def curate(body):
    with LOCK:
        STATE["curate"] += 1
    messages = body.get("messages") or []
    system = next((m.get("content") or "" for m in messages if m.get("role") == "system"), "")
    user = next((m for m in reversed(messages) if m.get("role") == "user"), {})
    content = user.get("content")
    parts = content if isinstance(content, list) else [{"type": "text", "text": str(content or "")}]
    text = " ".join(p.get("text", "") for p in parts if p.get("type") == "text")
    images = [p for p in parts if p.get("type") == "image_url"]
    for p in images:
        url = (p.get("image_url") or {}).get("url", "")
        m = re.match(r"^data:(image/[a-z]+);base64,(.+)$", url, re.S)
        if not m:
            raise ValueError("curate: every image must be an inline base64 data URL")
        raw = base64.b64decode(m.group(2), validate=True)
        if not raw.startswith(MAGIC.get(m.group(1), (b"\x00\x00\x00\x00",))):
            raise ValueError(f"curate: the bytes are not a {m.group(1)}")
    n = len(images)
    if n == 0:
        return {"scores": [], "best": 0, "why": "There are no photos to judge."}
    if "reject" in (system + " " + text).lower():
        return {"scores": [2] * n, "best": 0, "why": "None of these would work as a calm, on-theme background."}
    best = 2 if n >= 2 else 1
    scores = [5] * n
    scores[best - 1] = 9
    return {"scores": scores, "best": best, "why": f"Photo {best} has the warmest light and a calm area for text."}


CHAT_STAGES = {"curate": curate}


def handle(req):
    if req.path.startswith("/img/"):
        return image(req)
    if req.path.startswith("/openverse/"):
        return openverse(req)
    if req.path.startswith("/commons/"):
        return commons(req)
    return not_found(req.path)
