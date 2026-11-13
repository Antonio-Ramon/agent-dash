//! Leitor mínimo de SQLite, só leitura: percorre a b-tree de uma tabela e
//! devolve as linhas. Aplica o `-wal` (as páginas confirmadas mais novas
//! valem no lugar das do arquivo principal), porque os bancos lidos aqui ficam
//! em modo WAL enquanto o programa dono está aberto.
//!
//! Existe para não depender de uma biblioteca em C só para ler alguns
//! bancos de outros programas. Não trata índices, tabelas `WITHOUT ROWID` nem
//! bancos criptografados; qualquer coisa inesperada vira `None`.
//!
//! Formato: <https://www.sqlite.org/fileformat2.html>.

use std::collections::{HashMap, HashSet};
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Value::Blob(b) => b,
            Value::Text(s) => s.as_bytes(),
            _ => &[],
        }
    }
}

/// Uma linha: rowid e colunas na ordem da tabela. Numa coluna
/// `INTEGER PRIMARY KEY` o valor guardado é `Null` e o valor é o rowid.
pub type Row = (i64, Vec<Value>);

pub struct Db {
    main: Vec<u8>,
    wal: Vec<u8>,
    /// página -> posição dos dados dela dentro do `wal`.
    wal_pages: HashMap<u32, usize>,
    page_size: usize,
    usable: usize,
}

fn be16(b: &[u8], i: usize) -> Option<usize> {
    Some(u16::from_be_bytes(b.get(i..i + 2)?.try_into().ok()?) as usize)
}

fn be32(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// Varint do SQLite (big-endian, até 9 bytes). Devolve o valor e o tamanho.
fn varint(b: &[u8], i: usize) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for n in 0..9 {
        let x = *b.get(i + n)?;
        if n == 8 {
            return Some(((v << 8) | x as u64, 9));
        }
        v = (v << 7) | (x & 0x7f) as u64;
        if x < 0x80 {
            return Some((v, n + 1));
        }
    }
    None
}

/// Soma de verificação do WAL sobre `data` (múltiplo de 8 bytes).
fn wal_checksum(data: &[u8], big_endian: bool, mut s: (u32, u32)) -> (u32, u32) {
    for w in data.chunks_exact(8) {
        let word = |b: &[u8]| {
            let a: [u8; 4] = b.try_into().unwrap();
            if big_endian { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) }
        };
        s.0 = s.0.wrapping_add(word(&w[..4])).wrapping_add(s.1);
        s.1 = s.1.wrapping_add(word(&w[4..])).wrapping_add(s.0);
    }
    s
}

/// Páginas confirmadas do WAL: só quadros com o sal do cabeçalho, com a soma
/// encadeada certa e até o último quadro de commit.
fn wal_index(wal: &[u8]) -> Option<(usize, HashMap<u32, usize>)> {
    let magic = be32(wal, 0)?;
    if magic & !1 != 0x377f_0682 {
        return None;
    }
    let big = magic & 1 == 1;
    let page_size = be32(wal, 8)? as usize;
    if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
        return None;
    }
    let mut sum = wal_checksum(wal.get(..24)?, big, (0, 0));
    if (sum.0, sum.1) != (be32(wal, 24)?, be32(wal, 28)?) {
        return None;
    }
    let salt = wal.get(16..24)?;
    let mut pages = HashMap::new();
    let mut pending = Vec::new();
    let mut off = 32;
    while off + 24 + page_size <= wal.len() {
        let h = &wal[off..off + 24];
        if &h[8..16] != salt {
            break;
        }
        sum = wal_checksum(&h[..8], big, sum);
        sum = wal_checksum(&wal[off + 24..off + 24 + page_size], big, sum);
        if (sum.0, sum.1) != (be32(h, 16)?, be32(h, 20)?) {
            break;
        }
        pending.push((be32(h, 0)?, off + 24));
        if be32(h, 4)? != 0 {
            // Commit: as páginas até aqui passam a valer.
            pages.extend(pending.drain(..));
        }
        off += 24 + page_size;
    }
    Some((page_size, pages))
}

impl Db {
    /// Lê `path` e, se existir, `path-wal`. Os arquivos são lidos inteiros: é
    /// para bancos pequenos.
    pub fn open(path: &Path) -> Option<Db> {
        let main = std::fs::read(path).ok()?;
        let mut wal_path = path.as_os_str().to_owned();
        wal_path.push("-wal");
        let wal = std::fs::read(&wal_path).unwrap_or_default();
        Self::from_bytes(main, wal)
    }

    pub fn from_bytes(main: Vec<u8>, wal: Vec<u8>) -> Option<Db> {
        let (wal_ps, wal_pages) = wal_index(&wal).unwrap_or((0, HashMap::new()));
        let mut db = Db { main, wal, wal_pages, page_size: 0, usable: 0 };
        let ps = if db.main.len() >= 100 && db.main.starts_with(b"SQLite format 3\0") {
            match be16(&db.main, 16)? {
                1 => 65536,
                n => n,
            }
        } else if wal_ps > 0 {
            wal_ps
        } else {
            return None;
        };
        db.page_size = ps;
        let p1 = db.page(1)?;
        if !p1.starts_with(b"SQLite format 3\0") {
            return None;
        }
        db.usable = ps - *p1.get(20)? as usize;
        Some(db)
    }

    fn page(&self, n: u32) -> Option<&[u8]> {
        if n == 0 {
            return None;
        }
        if let Some(&off) = self.wal_pages.get(&n) {
            return self.wal.get(off..off + self.page_size);
        }
        let start = (n as usize - 1) * self.page_size;
        self.main.get(start..start + self.page_size)
    }

    /// Todas as linhas da tabela `name`, em ordem de rowid.
    pub fn table(&self, name: &str) -> Option<Vec<Row>> {
        let schema = self.rows(1)?;
        let root = schema.iter().find_map(|(_, c)| {
            (c.first()?.as_text()? == "table" && c.get(1)?.as_text()? == name).then(|| c.get(3)?.as_int())?
        })?;
        self.rows(u32::try_from(root).ok()?)
    }

    /// Percorre a b-tree de tabela com raiz em `root`.
    fn rows(&self, root: u32) -> Option<Vec<Row>> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        let mut seen = HashSet::new();
        while let Some(pg) = stack.pop() {
            if !seen.insert(pg) {
                return None; // ciclo: banco inconsistente (lido no meio de uma escrita?)
            }
            let page = self.page(pg)?;
            let h = if pg == 1 { 100 } else { 0 };
            let kind = *page.get(h)?;
            let cells = be16(page, h + 3)?;
            match kind {
                0x0d => {
                    let mut leaf = Vec::with_capacity(cells);
                    for i in 0..cells {
                        let at = be16(page, h + 8 + 2 * i)?;
                        leaf.push(self.leaf_cell(page, at)?);
                    }
                    out.push(leaf);
                }
                0x05 => {
                    // Filhos da direita para a esquerda na pilha: a ordem de saída fica crescente.
                    stack.push(be32(page, h + 8)?);
                    for i in (0..cells).rev() {
                        let at = be16(page, h + 12 + 2 * i)?;
                        stack.push(be32(page, at)?);
                    }
                }
                _ => return None,
            }
        }
        Some(out.into_iter().flatten().collect())
    }

    fn leaf_cell(&self, page: &[u8], at: usize) -> Option<Row> {
        let (size, n1) = varint(page, at)?;
        let (rowid, n2) = varint(page, at + n1)?;
        let size = usize::try_from(size).ok()?;
        let start = at + n1 + n2;
        let u = self.usable;
        let max_local = u - 35;
        let local = if size <= max_local {
            size
        } else {
            let min_local = (u - 12) * 32 / 255 - 23;
            let k = min_local + (size - min_local) % (u - 4);
            if k <= max_local { k } else { min_local }
        };
        let mut payload = page.get(start..start + local)?.to_vec();
        if local < size {
            let mut next = be32(page, start + local)?;
            let mut hops = 0;
            while payload.len() < size {
                hops += 1;
                if next == 0 || hops > 100_000 {
                    return None;
                }
                let ov = self.page(next)?;
                let take = (size - payload.len()).min(u - 4);
                payload.extend_from_slice(ov.get(4..4 + take)?);
                next = be32(ov, 0)?;
            }
        }
        Some((rowid as i64, record(&payload)?))
    }
}

/// Decodifica um registro (cabeçalho de tipos + valores).
fn record(p: &[u8]) -> Option<Vec<Value>> {
    let (hlen, n) = varint(p, 0)?;
    let hlen = usize::try_from(hlen).ok()?;
    let mut types = Vec::new();
    let mut i = n;
    while i < hlen {
        let (t, n) = varint(p, i)?;
        types.push(t);
        i += n;
    }
    let mut body = hlen;
    let mut out = Vec::with_capacity(types.len());
    for t in types {
        let int = |len: usize| -> Option<i64> {
            let b = p.get(body..body + len)?;
            let mut v: i64 = if b[0] & 0x80 != 0 { -1 } else { 0 };
            for x in b {
                v = (v << 8) | *x as i64;
            }
            Some(v)
        };
        let (v, len) = match t {
            0 => (Value::Null, 0),
            1..=4 => (Value::Int(int(t as usize)?), t as usize),
            5 => (Value::Int(int(6)?), 6),
            6 => (Value::Int(int(8)?), 8),
            7 => (Value::Real(f64::from_bits(int(8)? as u64)), 8),
            8 => (Value::Int(0), 0),
            9 => (Value::Int(1), 0),
            10 | 11 => return None,
            n if n % 2 == 0 => {
                let len = ((n - 12) / 2) as usize;
                (Value::Blob(p.get(body..body + len)?.to_vec()), len)
            }
            n => {
                let len = ((n - 13) / 2) as usize;
                (Value::Text(String::from_utf8_lossy(p.get(body..body + len)?).into_owned()), len)
            }
        };
        out.push(v);
        body += len;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_do_sqlite() {
        assert_eq!(varint(&[0x05], 0), Some((5, 1)));
        assert_eq!(varint(&[0x81, 0x00], 0), Some((128, 2)));
        assert_eq!(varint(&[0xff; 9], 0), Some((u64::MAX, 9)));
        assert_eq!(varint(&[0x81], 0), None);
    }

    #[test]
    fn registro_com_todos_os_tipos() {
        // cabeçalho: 8 bytes (o próprio tamanho + 7 tipos): NULL, int8, int16, 0, 1, blob(2), texto(2)
        let p = [8, 0, 1, 2, 8, 9, 16, 17, 0xfe, 0x01, 0x00, 0xaa, 0xbb, b'o', b'i'];
        let r = record(&p).unwrap();
        assert_eq!(
            r,
            vec![Value::Null, Value::Int(-2), Value::Int(256), Value::Int(0), Value::Int(1), Value::Blob(vec![0xaa, 0xbb]), Value::Text("oi".into())]
        );
    }

    #[test]
    fn lixo_vira_none() {
        assert!(Db::from_bytes(b"isto nao e sqlite".to_vec(), Vec::new()).is_none());
        assert!(Db::from_bytes(Vec::new(), Vec::new()).is_none());
    }
}
