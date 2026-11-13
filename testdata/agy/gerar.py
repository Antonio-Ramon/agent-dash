"""Gera a conversa falsa do Antigravity usada nos testes (`conv.db` + `conv.db-wal`).

Mesmo esquema das tabelas `steps` e `gen_metadata` do `agy` 1.1, com páginas
de 512 bytes para forçar b-tree com páginas internas e overflow. Parte das
linhas fica só no `-wal` (não confirmada no arquivo principal), como acontece
enquanto o `agy` está com a conversa aberta.

Uso: python gerar.py  (escreve ao lado deste arquivo e imprime os totais
esperados pelos testes).
"""

import os
import shutil
import sqlite3
import tempfile

AQUI = os.path.dirname(os.path.abspath(__file__))


def varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def campo_int(num, v):
    return varint(num << 3) + varint(v)


def campo_bytes(num, b):
    return varint((num << 3) | 2) + varint(len(b)) + b


def metadata(segundos, uso=None):
    m = campo_bytes(1, campo_int(1, segundos) + campo_int(2, 500))
    m += campo_int(3, 2)
    if uso:
        m += campo_bytes(9, b"".join(campo_int(k, v) for k, v in uso))
    return m


def main():
    tmp = tempfile.mkdtemp()
    db = os.path.join(tmp, "conv.db")
    con = sqlite3.connect(db)
    con.execute("pragma page_size=512")
    con.execute("pragma journal_mode=wal")
    con.execute("pragma wal_autocheckpoint=0")
    con.execute(
        "create table `steps` (`idx` integer,`step_type` integer NOT NULL DEFAULT 0,`status` integer NOT NULL DEFAULT 0,"
        "`has_subtrajectory` numeric NOT NULL DEFAULT false,`metadata` blob,`error_details` blob,`permissions` blob,"
        "`task_details` blob,`render_info` blob,`step_payload` blob,`step_format` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`))"
    )
    con.execute("create table `gen_metadata` (`idx` integer,`data` blob,`size` integer NOT NULL DEFAULT 0,PRIMARY KEY (`idx`))")
    cfg = campo_int(1, 1318) + campo_bytes(28, b"gemini-teste")
    con.execute("insert into gen_metadata values (0, ?, 0)", (campo_bytes(3, cfg),))

    esperado = {"respostas": 0, "entrada": 0, "saida": 0, "cache": 0}
    base = 1794510000

    def passo(i):
        resposta = i % 3 == 1
        uso = None
        if resposta:
            entrada, pensa, fala, cache = 1000 + i, 100 + i, 10 + i, (i * 7 if i % 2 else 0)
            uso = [(1, 1318 if i % 4 else 1050), (2, entrada), (3, pensa + fala), (5, cache), (9, pensa), (10, fala)]
            esperado["respostas"] += 1
            esperado["entrada"] += entrada
            esperado["saida"] += pensa + fala
            esperado["cache"] += cache
        payload = bytes((i + k) % 251 for k in range(1500))  # vai para páginas de overflow
        con.execute(
            "insert into steps (idx, step_type, metadata, step_payload) values (?, ?, ?, ?)",
            (i, 15 if resposta else 14, metadata(base + i * 60, uso), payload),
        )

    for i in range(30):
        passo(i)
    con.commit()
    con.execute("pragma wal_checkpoint(truncate)")  # estas 30 vão para o arquivo principal
    for i in range(30, 45):
        passo(i)
    con.commit()  # estas 15 ficam só no -wal

    # Copia com a conexão aberta: fechar faria o checkpoint e apagaria o -wal.
    shutil.copy(db, os.path.join(AQUI, "conv.db"))
    shutil.copy(db + "-wal", os.path.join(AQUI, "conv.db-wal"))
    con.close()
    print(esperado)


if __name__ == "__main__":
    main()
