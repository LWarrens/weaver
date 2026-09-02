import sqlite3, sys, json, os

db_path = os.path.join(os.path.dirname(__file__), '..', 'arch.db')
db = sqlite3.connect(db_path)
db.row_factory = sqlite3.Row

# --- repos ---
repos = db.execute("SELECT id, path FROM repositories").fetchall()
print(f"\n=== REPOS ({len(repos)}) ===")
for r in repos:
    print(f"  {r['id'][:8]}  {r['path']}")

# --- decisions: lead vs adr-backed ---
print("\n=== DECISIONS ===")
rows = db.execute("""
    SELECT d.id, d.title, d.text,
           d.valid_from, d.valid_to,
           CASE WHEN ad.id IS NOT NULL THEN 'decision' ELSE 'lead' END AS kind,
           ad.adr_id
    FROM decisions d
    LEFT JOIN adr_documents ad ON ad.id = d.adr_id
    WHERE d.valid_to IS NULL
    ORDER BY d.valid_from DESC
""").fetchall()
leads   = [r for r in rows if r['kind'] == 'lead']
decisions = [r for r in rows if r['kind'] == 'decision']
print(f"  Active leads:     {len(leads)}")
print(f"  Active decisions: {len(decisions)}")

# --- per-lead: entity links, code links, embeddings ---
print("\n=== LEAD DETAIL ===")
for r in leads[:20]:
    did = r['id']

    # entity node links via temporal edges
    entity_links = db.execute("""
        SELECT te.target_id, en.canonical_name, en.embedding IS NOT NULL AS has_emb
        FROM temporal_edges te
        LEFT JOIN entity_nodes en ON en.id = te.target_id
        WHERE te.source_id = ? AND te.source_type = 'decision' AND te.edge_type = 'mentions'
    """, (did,)).fetchall()

    # file links via decision_code_links
    file_links = db.execute("""
        SELECT file_path, symbol, link_type FROM decision_code_links
        WHERE decision_id = ? AND valid_to IS NULL
    """, (did,)).fetchall()

    # embedding on decision itself
    has_emb = db.execute(
        "SELECT embedding IS NOT NULL AS has_emb FROM decisions WHERE id = ?", (did,)
    ).fetchone()['has_emb']

    title = (r['title'] or '')[:60]
    print(f"\n  [{did[:8]}] {title}")
    print(f"    embedding:    {'yes' if has_emb else 'NO'}")
    print(f"    entity links: {len(entity_links)}  (with emb: {sum(e['has_emb'] or 0 for e in entity_links)})")
    print(f"    file links:   {len(file_links)}")
    for fl in file_links[:3]:
        sym = fl['symbol'] or '(no symbol)'
        print(f"      {fl['link_type']} {fl['file_path']} · {sym}")

# --- symbols in repo vs entity nodes ---
print("\n=== SYMBOL COVERAGE ===")
sym_count = db.execute("SELECT COUNT(*) FROM symbols WHERE valid_to IS NULL").fetchone()[0]
entity_count = db.execute("SELECT COUNT(*) FROM entity_nodes").fetchone()[0]
entity_emb = db.execute("SELECT COUNT(*) FROM entity_nodes WHERE embedding IS NOT NULL").fetchone()[0]
dec_emb = db.execute("SELECT COUNT(*) FROM decisions WHERE embedding IS NOT NULL AND valid_to IS NULL").fetchone()[0]
print(f"  Symbols indexed:          {sym_count}")
print(f"  Entity nodes:             {entity_count}  ({entity_emb} with embeddings)")
print(f"  Decisions with embedding: {dec_emb} / {len(rows)}")
