//! [`Table`] — left-aligned columns separated by two spaces.

/// A plain-text table.
#[derive(Debug)]
pub(crate) struct Table {
    rows: Vec<Vec<String>>,
}

impl Table {
    /// A table with a header row.
    pub(crate) fn new(header: &[&str]) -> Self {
        Self {
            rows: vec![header.iter().map(|h| (*h).to_owned()).collect()],
        }
    }

    /// Append a row.
    pub(crate) fn row(&mut self, cells: Vec<String>) {
        self.rows.push(cells);
    }

    /// Render with aligned columns; the last column is not padded.
    pub(crate) fn render(&self) -> String {
        let columns = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0; columns];
        for row in &self.rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        let mut out = String::new();
        for row in &self.rows {
            let mut line = String::new();
            for (i, cell) in row.iter().enumerate() {
                if i + 1 == row.len() {
                    line.push_str(cell);
                } else {
                    line.push_str(cell);
                    let pad = widths[i] - cell.chars().count() + 2;
                    line.extend(std::iter::repeat_n(' ', pad));
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligns_columns() {
        let mut table = Table::new(&["ID", "TITLE"]);
        table.row(vec!["#12".into(), "x".into()]);
        assert_eq!(table.render(), "ID   TITLE\n#12  x\n");
    }
}
