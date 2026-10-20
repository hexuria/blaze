// Colours for the Rust in the editor: `paint(text)` is the text as HTML, a `<span>` around
// each token that has a colour. It reads tokens only, so it never disagrees with the text.

const KEYWORDS = new Set(
  ("as async await break const continue crate dyn else enum extern false fn for if impl in let " +
    "loop match mod move mut pub ref return self Self static struct super trait true type union " +
    "unsafe use where while").split(" "),
);
const PRIMITIVES = new Set(
  "bool char str u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64".split(" "),
);
const TOKEN = new RegExp(
  [
    /(?<comment>\/\/[^\n]*|\/\*[\s\S]*?(?:\*\/|$))/,
    /(?<string>b?r(?<hashes>#*)"[\s\S]*?(?:"\k<hashes>|$)|b?"(?:\\[\s\S]|[^"\\])*(?:"|$)|b?'(?:\\(?:u\{[^}\n]*\}|[^\n])|[^'\\\n])')/,
    /(?<attribute>#!?\[[^\]\n]*\])/,
    /(?<lifetime>'[A-Za-z_]\w*)/,
    /(?<number>\b\d\w*(?:\.\d\w*)?)/,
    /(?<word>\b[A-Za-z_]\w*)/,
  ]
    .map((part) => part.source)
    .join("|"),
  "g",
);

const escape = (text) => text.replace(/[&<>]/g, (it) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[it]);

function kind(groups, text, end) {
  if (groups.word === undefined) {
    return ["comment", "string", "attribute", "lifetime", "number"].find((it) => groups[it] !== undefined);
  }
  const word = groups.word;
  if (KEYWORDS.has(word)) {
    return "keyword";
  }
  if (text[end] === "!" && text[end + 1] !== "=") {
    return "macro";
  }
  if (PRIMITIVES.has(word) || /^[A-Z]/.test(word)) {
    return "type";
  }
  return /^\s*\(/.test(text.slice(end, end + 8)) ? "function" : undefined;
}

export function paint(text) {
  let html = "";
  let at = 0;
  for (const match of text.matchAll(TOKEN)) {
    const end = match.index + match[0].length;
    const name = kind(match.groups, text, end);
    html += escape(text.slice(at, match.index));
    html += name ? `<span class="t-${name}">${escape(match[0])}</span>` : escape(match[0]);
    at = end;
  }
  // A last line that is empty has a height in the textarea, and has one here with this.
  return `${html}${escape(text.slice(at))}\n`;
}
