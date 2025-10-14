#!/usr/bin/env python3
import os
import json
import sqlite3

# Folder containing your JSON files
JSON_FOLDER = "/home/query/Dev/cluetrainer/static/map/collision/"
DB_FILE = "tiles.db"

def create_tables(conn):
    cur = conn.cursor()
    cur.execute("""
        CREATE TABLE IF NOT EXISTS tiles (
            x INTEGER,
            y INTEGER,
            plane INTEGER,
            tiledata INTEGER,
            category TEXT,
            allowed_directions TEXT,
            blocked_directions TEXT,
            PRIMARY KEY (x, y, plane)
        )
    """)
    conn.commit()

def insert_tiles(conn, tiles):
    cur = conn.cursor()
    for tile in tiles:
        cur.execute("""
            INSERT OR REPLACE INTO tiles 
            (x, y, plane, tiledata, category, allowed_directions, blocked_directions)
            VALUES (?, ?, ?, ?, ?, ?, ?)
        """, (
            tile["x"],
            tile["y"],
            tile["z"],  # becomes plane
            tile["tiledata"],
            tile["classification"]["category"],
            ",".join(tile["classification"]["allowed_directions"]),
            ",".join(tile["classification"]["blocked_directions"])
        ))
    conn.commit()

def load_json_files(folder, conn):
    for filename in os.listdir(folder):
        if filename.endswith(".json"):
            filepath = os.path.join(folder, filename)
            print(f"Loading {filepath}...")
            with open(filepath, "r") as f:
                data = json.load(f)
            insert_tiles(conn, data["tiles"])

def main():
    conn = sqlite3.connect(DB_FILE)
    create_tables(conn)
    load_json_files(JSON_FOLDER, conn)
    conn.close()
    print(f"Tiles successfully loaded into {DB_FILE}")

if __name__ == "__main__":
    main()
