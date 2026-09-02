import sqlite3
from pathlib import Path

DB = Path('arch.db')
if not DB.exists():
    print('arch.db not found')
    raise SystemExit(1)

conn = sqlite3.connect(str(DB))
cur = conn.cursor()

def count(table):
    try:
        cur.execute(f"SELECT COUNT(*) FROM {table}")
        return cur.fetchone()[0]
    except Exception as e:
        return f'ERR: {e}'

print('files:', count('files'))
print('symbols:', count('symbols'))
print('adrs:', count('adr_documents'))

print('\nSample files containing ".claude":')
for row in cur.execute("SELECT path FROM files WHERE path LIKE '%.claude%' LIMIT 20"):
    print(row[0])

print('\nSample files under src/:')
for row in cur.execute("SELECT path FROM files WHERE path LIKE 'src/%' LIMIT 20"):
    print(row[0])

conn.close()
