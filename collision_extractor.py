from __future__ import annotations

"""RuneScape collision data extraction utilities in Python.

This module mirrors the functionality previously provided by the Kotlin
prototype and can be used directly or imported into other projects.
"""

import gzip
import json
import logging
import re
import time
import zlib
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, Iterable, List, Optional, Tuple
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

# Configure a basic logger for convenience when running as a script.
logging.basicConfig(level=logging.INFO, format="%(levelname)s: %(message)s")


@dataclass(frozen=True)
class TileCoordinates:
    """Represents a RuneScape tile coordinate."""

    x: int
    y: int  # Commonly referred to as Z in RuneScape resources
    level: int

    def as_dict(self) -> Dict[str, int]:
        return {"x": self.x, "y": self.y, "level": self.level}


class CollisionDataManager:
    """Downloads, caches, and exports RuneScape collision data."""

    BASE_URL_TEMPLATE: str = "https://runeapps.org/s3/map4/{version}"
    BACKUP_URL_TEMPLATE: str = (
        "https://runeapps.org/node/map/getnamed?mapid=4&version={version}&file={filename}"
    )
    DEFAULT_MAP_VERSION: int = 1710516666

    CHUNKS_X: int = 100
    CHUNKS_Y: int = 200
    CHUNK_SIZE: int = 64
    CHUNKS_PER_FILE: int = 20
    LEVELS: int = 4

    def __init__(
        self,
        user_agent: str = "ClueTrainer-Python/1.0",
        map_version: int = DEFAULT_MAP_VERSION,
    ) -> None:
        self._user_agent = user_agent
        self._map_version = map_version
        file_count = (self.CHUNKS_X // self.CHUNKS_PER_FILE) * (
            self.CHUNKS_Y // self.CHUNKS_PER_FILE
        )
        self._cache: List[Dict[int, bytes]] = [{ } for _ in range(self.LEVELS)]
        self._file_count = file_count

    # ------------------------------------------------------------------
    # Low-level data handling
    # ------------------------------------------------------------------
    def _collision_filename(self, file_x: int, file_y: int, level: int) -> str:
        return f"collision-{file_x}-{file_y}-{level}.bin"

    def _download_collision_file(self, file_x: int, file_y: int, level: int) -> bytes:
        filename = self._collision_filename(file_x, file_y, level)
        versioned_url = f"{self.BASE_URL_TEMPLATE.format(version=self._map_version)}/{filename}"
        live_url = f"{self.BASE_URL_TEMPLATE.format(version='live')}/{filename}"
        backup_versioned = self.BACKUP_URL_TEMPLATE.format(
            version=self._map_version,
            filename=filename,
        )
        backup_live = self.BACKUP_URL_TEMPLATE.format(version="live", filename=filename)

        urls = [versioned_url, live_url, backup_versioned, backup_live]

        for attempt, url in enumerate(urls, start=1):
            request = Request(url, headers={"User-Agent": self._user_agent})
            try:
                logging.debug("Fetching collision file from %s", url)
                with urlopen(request, timeout=30) as response:
                    compressed = response.read()
                    return gzip.decompress(compressed)
            except HTTPError as exc:
                logging.warning(
                    "Attempt %d failed for %s with HTTP %s", attempt, url, exc.code
                )
            except URLError as exc:
                logging.warning(
                    "Attempt %d failed for %s with error %s", attempt, url, exc.reason
                )
            except OSError as exc:
                logging.warning(
                    "Attempt %d failed for %s with error %s", attempt, url, exc
                )

        raise RuntimeError(f"Unable to download collision data for {filename}")

    def _get_file_bytes(self, file_x: int, file_y: int, level: int) -> bytes:
        file_index = self._file_index(file_x, file_y)
        level_cache = self._cache[level]

        if file_index not in level_cache:
            level_cache[file_index] = self._download_collision_file(file_x, file_y, level)

        return level_cache[file_index]

    def _file_index(self, file_x: int, file_y: int) -> int:
        files_per_row = self.CHUNKS_X // self.CHUNKS_PER_FILE
        return file_y * files_per_row + file_x

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------
    def get_tile(self, coords: TileCoordinates) -> int:
        """Return the 8-bit collision value for the requested tile."""

        if not self._is_valid_coordinate(coords):
            raise ValueError(f"Invalid coordinates: {coords}")

        chunk_span = self.CHUNK_SIZE * self.CHUNKS_PER_FILE
        file_x = coords.x // chunk_span
        file_y = coords.y // chunk_span

        file_bytes = self._get_file_bytes(file_x, file_y, coords.level)

        tile_x = coords.x % chunk_span
        tile_y = coords.y % chunk_span
        tile_index = tile_y * chunk_span + tile_x

        return file_bytes[tile_index]

    def extract_area(
        self,
        top_left: TileCoordinates,
        width: int,
        height: int,
        level: Optional[int] = None,
    ) -> Dict[TileCoordinates, int]:
        """Extract collision values for a rectangular area."""

        if level is None:
            level = top_left.level

        area: Dict[TileCoordinates, int] = {}
        for dy in range(height):
            for dx in range(width):
                coords = TileCoordinates(top_left.x + dx, top_left.y + dy, level)
                area[coords] = self.get_tile(coords)

        return area

    def export_area_binary(
        self,
        output_path: Path | str,
        area_bounds: Tuple[TileCoordinates, TileCoordinates],
    ) -> Path:
        """Export collision data for the specified area in binary format."""

        path = Path(output_path)
        start, end = area_bounds
        width = end.x - start.x + 1
        height = end.y - start.y + 1

        header = [
            width >> 8,
            width & 0xFF,
            height >> 8,
            height & 0xFF,
            start.x >> 8,
            start.x & 0xFF,
            start.y >> 8,
            start.y & 0xFF,
            start.level & 0xFF,
        ]

        payload = bytearray(header)
        for y in range(start.y, start.y + height):
            for x in range(start.x, start.x + width):
                coords = TileCoordinates(x, y, start.level)
                payload.append(self.get_tile(coords))

        path.write_bytes(payload)
        return path

    def export_area_json(
        self,
        output_path: Path | str,
        area_bounds: Tuple[TileCoordinates, TileCoordinates],
        pretty: bool = True,
    ) -> Path:
        """Export collision data for the specified area as JSON."""

        path = Path(output_path)
        start, end = area_bounds
        width = end.x - start.x + 1
        height = end.y - start.y + 1

        tiles: List[Dict[str, Any]] = []
        for y in range(start.y, start.y + height):
            for x in range(start.x, start.x + width):
                coords = TileCoordinates(x, y, start.level)
                tile_value = self.get_tile(coords)
                if tile_value == 0xFF:
                    continue
                tiles.append(
                    {
                        "x": x,
                        "y": y,
                        "z": start.level,
                        "tiledata": tile_value,
                        "classification": _classify_tiledata(tile_value),
                    }
                )

        data = {
            "metadata": {
                "start": start.as_dict(),
                "width": width,
                "height": height,
            },
            "tiles": tiles,
        }

        path.write_text(json.dumps(data, indent=2 if pretty else None))
        return path

    def load_exported_binary(
        self, input_path: Path | str
    ) -> Tuple[Dict[TileCoordinates, int], Dict[str, int]]:
        """Load collision data from a previously exported binary file."""

        path = Path(input_path)
        raw = path.read_bytes()

        if len(raw) < 9:
            raise ValueError("Binary file too short to contain header")

        width = (raw[0] << 8) | raw[1]
        height = (raw[2] << 8) | raw[3]
        start_x = (raw[4] << 8) | raw[5]
        start_y = (raw[6] << 8) | raw[7]
        level = raw[8]

        payload = raw[9:]
        expected = width * height
        if len(payload) != expected:
            raise ValueError(
                f"Binary file payload length {len(payload)} does not match expected {expected}"
            )

        start = TileCoordinates(start_x, start_y, level)
        area: Dict[TileCoordinates, int] = {}

        index = 0
        for y in range(start.y, start.y + height):
            for x in range(start.x, start.x + width):
                coords = TileCoordinates(x, y, level)
                area[coords] = payload[index]
                index += 1

        metadata = {
            "start": start.as_dict(),
            "width": width,
            "height": height,
        }

        return area, metadata

    # ------------------------------------------------------------------
    # Utility helpers
    # ------------------------------------------------------------------
    def _is_valid_coordinate(self, coords: TileCoordinates) -> bool:
        return (
            0 <= coords.x < self.CHUNK_SIZE * self.CHUNKS_X
            and 0 <= coords.y < self.CHUNK_SIZE * self.CHUNKS_Y
            and 0 <= coords.level < self.LEVELS
        )

DIRECTION_BITS: Dict[str, int] = {
    "west": 0,
    "north": 1,
    "east": 2,
    "south": 3,
    "northwest": 4,
    "northeast": 5,
    "southeast": 6,
    "southwest": 7,
}


def _decode_collision_bytes(raw: bytes) -> bytes:
    """Return the decompressed collision payload from a .bin file."""

    for decoder in (
        lambda b: zlib.decompress(b),
        lambda b: zlib.decompress(b, wbits=-zlib.MAX_WBITS),
        lambda b: gzip.decompress(b),
    ):
        try:
            return decoder(raw)
        except OSError:
            continue
        except zlib.error:
            continue

    raise ValueError("Unable to decompress collision binary payload")


def _classify_tiledata(value: int) -> Dict[str, object]:
    """Provide a human-friendly classification of a collision tile value."""

    allowed = [direction for direction, bit in DIRECTION_BITS.items() if value & (1 << bit)]
    blocked = [direction for direction in DIRECTION_BITS if direction not in allowed]

    if value == 0:
        category = "blocked"
    elif value == 0xFF:
        category = "fully_open"
    else:
        category = "partially_open"

    return {
        "category": category,
        "allowed_directions": allowed,
        "blocked_directions": blocked,
    }


def convert_collision_bin_to_json(
    bin_path: Path | str,
    output_path: Path | str | None = None,
    pretty: bool = True,
) -> Path:
    """Read a local collision .bin file, decode it, and write a JSON summary."""

    path = Path(bin_path)
    if not path.is_file():
        raise FileNotFoundError(path)

    match = re.search(r"collision-(\d+)-(\d+)-(\d+)\.bin", path.name)
    if not match:
        raise ValueError(f"Filename does not follow expected pattern: {path.name}")

    file_x, file_y, level = map(int, match.groups())

    raw = path.read_bytes()
    payload = _decode_collision_bytes(raw)

    width = CollisionDataManager.CHUNK_SIZE * CollisionDataManager.CHUNKS_PER_FILE
    height = width  # Square tiles per file

    expected_len = width * height
    if len(payload) != expected_len:
        raise ValueError(
            f"Decoded payload length {len(payload)} does not match expected {expected_len}"
        )

    start_x = file_x * width
    start_y = file_y * height

    tiles: List[Dict[str, Any]] = []
    for index, value in enumerate(payload):
        row = index // width
        col = index % width
        tiles.append(
            {
                "x": start_x + col,
                "y": start_y + row,
                "z": level,
                "tiledata": value,
                "classification": _classify_tiledata(value),
            }
        )

    json_data = {
        "metadata": {
            "file": path.name,
            "file_indices": {"x": file_x, "y": file_y, "level": level},
            "start": {"x": start_x, "y": start_y, "level": level},
            "width": width,
            "height": height,
        },
        "tiles": tiles,
    }

    if output_path is None:
        output_path = path.with_suffix(".json")

    output = Path(output_path)
    output.write_text(json.dumps(json_data, indent=2 if pretty else None))
    return output


def _demo() -> None:
    manager = CollisionDataManager()
    lumbridge = TileCoordinates(3222, 3218, 0)

    try:
        logging.info("Downloading sample collision data...")
        sample = manager.get_tile(lumbridge)
        logging.info("Collision value at Lumbridge Castle (%s): %s", lumbridge, sample)
    except RuntimeError:
        logging.warning("Remote download failed; continuing with local file demonstration")

    export_start = TileCoordinates(3220, 3216, 0)
    export_end = TileCoordinates(3230, 3226, 0)

    logging.info("Exporting 11x11 area around Lumbridge to binary and JSON")
    manager.export_area_binary("lumbridge_sample.bin", (export_start, export_end))
    manager.export_area_json("lumbridge_sample.json", (export_start, export_end))
    logging.info("Export complete -> lumbridge_sample.bin / lumbridge_sample.json")

    sample_bin = Path("static/map/collision/collision-0-0-0.bin")
    if sample_bin.is_file():
        logging.info("Converting %s to JSON", sample_bin)
        output_json = convert_collision_bin_to_json(sample_bin)
        logging.info("JSON written to %s", output_json)
    else:
        logging.warning("Sample local collision bin not found at %s", sample_bin)


if __name__ == "__main__":
    start_time = time.time()
    try:
        _demo()
    except Exception as exc:
        logging.error("Collision extraction failed: %s", exc)
        raise SystemExit(1) from exc
    else:
        duration = time.time() - start_time
        logging.info("Done in %.2fs", duration)
        raise SystemExit(0)
