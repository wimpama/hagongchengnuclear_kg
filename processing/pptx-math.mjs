// Office equations are not ordinary DrawingML text runs. Adapt supported OMML
// into measured inline objects, then paint their SVG paths in the slide's own
// text layout. No network, Office installation or model inference is involved.
import { DOMParser, XMLSerializer } from '@xmldom/xmldom'
import { unzipSync, zipSync, strFromU8, strToU8 } from 'fflate'
import { posix } from 'node:path'
import { mathjax } from 'mathjax-full/js/mathjax.js'
import { TeX } from 'mathjax-full/js/input/tex.js'
import { SVG } from 'mathjax-full/js/output/svg.js'
import { liteAdaptor } from 'mathjax-full/js/adaptors/liteAdaptor.js'
import { RegisterHTMLHandler } from 'mathjax-full/js/handlers/html.js'
import 'mathjax-full/js/input/tex/ams/AmsConfiguration.js'

const M = 'http://schemas.openxmlformats.org/officeDocument/2006/math'
const A = 'http://schemas.openxmlformats.org/drawingml/2006/main'
const MC = 'http://schemas.openxmlformats.org/markup-compatibility/2006'
const P = 'http://schemas.openxmlformats.org/presentationml/2006/main'
const R = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const PKG_R = 'http://schemas.openxmlformats.org/package/2006/relationships'
const S = 'http://www.w3.org/2000/svg'
const MAX_EQUATIONS = 2048
const MAX_TEX = 16384
const MAX_XML = 20 * 1024 * 1024
const serializer = new XMLSerializer()
const adaptor = liteAdaptor()
RegisterHTMLHandler(adaptor)

export class EquationError extends Error {}

function parseXml(xml) {
  if (xml.length > MAX_XML || /<!DOCTYPE|<!ENTITY/i.test(xml)) throw new EquationError('文档 XML 超出限制或包含不支持的声明')
  return new DOMParser({
    onError(level) { throw new EquationError(`文档 XML 无效（${level}）`) },
  }).parseFromString(xml, 'application/xml')
}

const elements = node => Array.from(node.childNodes || []).filter(child => child.nodeType === 1)
const first = (node, name) => elements(node || {}).find(child => child.namespaceURI === M && child.localName === name)
const value = (node, name, fallback = '') => first(node, name)?.getAttributeNS(M, 'val') ?? fallback
const truth = (node, name) => {
  const property = first(node, name)
  return !!property && !['0', 'false', 'off'].includes(property.getAttributeNS(M, 'val') ?? '1')
}
const isEquation = node => node.namespaceURI === M && ['oMath', 'oMathPara'].includes(node.localName)
const descendants = (node, ns, name) => Array.from(node.getElementsByTagNameNS(ns, name))

function atom(text) {
  const symbols = {
    '×': '\\times ', '∙': '\\cdot ', '·': '\\cdot ', '−': '-', '–': '-',
    '±': '\\pm ', '∓': '\\mp ', '∞': '\\infty ', '≤': '\\le ', '≥': '\\ge ',
    '≠': '\\ne ', '≈': '\\approx ', '∝': '\\propto ', '→': '\\rightarrow ',
    '↔': '\\leftrightarrow ', '∈': '\\in ', '∂': '\\partial ', '∇': '\\nabla ',
    '\\': '\\backslash ', '^': '\\text{\\^{}}', '~': '\\sim ', ' ': '\\ ',
  }
  return [...text].map(char => symbols[char] ?? ('{}_$%&#'.includes(char) ? `\\${char}` : char)).join('')
}

const DELIMITERS = new Map(Object.entries({
  '': '.', '(': '(', ')': ')', '[': '[', ']': ']', '{': '\\{', '}': '\\}',
  '|': '|', '‖': '\\Vert ', '⟨': '\\langle ', '⟩': '\\rangle ',
  '⌈': '\\lceil ', '⌉': '\\rceil ', '⌊': '\\lfloor ', '⌋': '\\rfloor ',
  '/': '/', '\\': '\\backslash ', '⎰': '\\lmoustache ', '⎱': '\\rmoustache ',
  '↑': '\\uparrow ', '↓': '\\downarrow ', '↕': '\\updownarrow ',
  '⇑': '\\Uparrow ', '⇓': '\\Downarrow ', '⇕': '\\Updownarrow ',
}))
const DELIMITER_ALIASES = new Map(Object.entries({
  '（': '(', '）': ')', '［': '[', '］': ']', '｛': '{', '｝': '}',
  '<': '⟨', '>': '⟩', '＜': '⟨', '＞': '⟩', '〈': '⟨', '〉': '⟩', '〈': '⟨', '〉': '⟩',
  '｜': '|', '∣': '|', '│': '|', '┃': '|', '∥': '‖', '║': '‖', '||': '‖',
}))

function delimiterToken(char) {
  return DELIMITERS.get(DELIMITER_ALIASES.get(char) ?? char)
}

function checkedCharacter(char, label) {
  if (DELIMITER_ALIASES.has(char) || char === '' || ([...char].length === 1 && !/[\p{Cc}\p{Cf}\p{Cs}]/u.test(char))) return char
  const sample = [...char].slice(0, 4)
  const codes = sample.map(value => `U+${value.codePointAt(0).toString(16).toUpperCase().padStart(4, '0')}`).join(' ')
  throw new EquationError(`${label}必须是单个可见字符，收到 ${JSON.stringify(sample.join(''))}（${codes}）`)
}

function characterProperty(properties, name, fallback) {
  // OMML distinguishes an omitted property (default fence) from an empty
  // property/val (no fence). Do not invent a parenthesis for an empty value.
  const property = first(properties, name)
  return property ? property.getAttributeNS(M, 'val') ?? '' : fallback
}

function delimitedFormula(node, render) {
  const properties = first(node, 'dPr')
  const left = checkedCharacter(characterProperty(properties, 'begChr', '('), '公式左括号')
  const right = checkedCharacter(characterProperty(properties, 'endChr', ')'), '公式右括号')
  const parts = elements(node).filter(child => child.namespaceURI === M && child.localName === 'e').map(render)
  let separator = ''
  // Separators can be punctuation such as commas and semicolons. They are not
  // all stretchable delimiters, and an unused separator needs no conversion.
  if (parts.length > 1) {
    const char = checkedCharacter(characterProperty(properties, 'sepChr', '|'), '公式参数分隔符')
    const token = delimiterToken(char)
    separator = !char ? '\\,' : token ? `\\middle${token} ` : `\\,${atom(char)}\\,`
  }
  const open = delimiterToken(left), close = delimiterToken(right)
  // OMML permits Unicode fences beyond TeX's stretchable alphabet. Preserve
  // their literal glyphs instead of rejecting the file or substituting a
  // mathematically different bracket; these uncommon glyphs stay normal size.
  const literalOpen = open === undefined ? `\\mathopen{\\text{${atom(left)}}}` : ''
  const literalClose = close === undefined ? `\\mathclose{\\text{${atom(right)}}}` : ''
  return `${literalOpen}\\left${open ?? '.'} ${parts.join(separator)} \\right${close ?? '.'}${literalClose}`
}

// Only known structures are accepted: concatenating unknown OMML atoms could
// silently turn a fraction, bound or isotope into a different equation.
export function ommlToLatex(node, depth = 0) {
  if (!node) return ''
  if (depth > 64) throw new EquationError('公式嵌套超过限制')
  if (node.namespaceURI !== M) {
    if (descendants(node, M, '*').length) throw new EquationError('公式含暂不支持的嵌套结构')
    return '' // Office run formatting is not content.
  }
  const kind = node.localName
  if (kind.endsWith('Pr') || ['aln', 'brk'].includes(kind)) return ''
  const render = child => ommlToLatex(child, depth + 1)
  const part = name => render(first(node, name))
  const join = () => elements(node).map(render).join('')
  let tex
  switch (kind) {
    case 't': tex = atom(node.textContent || ''); break
    case 'r': {
      tex = join()
      const properties = first(node, 'rPr')
      const script = value(properties, 'scr')
      const scriptCommand = { script: 'mathcal', fraktur: 'mathfrak', 'double-struck': 'mathbb', 'sans-serif': 'mathsf', monospace: 'mathtt' }[script]
      if (scriptCommand) tex = `\\${scriptCommand}{${tex}}`
      if (truth(properties, 'nor')) tex = `\\mathrm{${tex}}`
      else {
        const style = value(properties, 'sty')
        const command = { b: 'mathbf', bi: 'boldsymbol', p: 'mathrm' }[style]
        if (command) tex = `\\${command}{${tex}}`
      }
      break
    }
    case 'oMath': case 'oMathPara': case 'e': case 'num': case 'den':
    case 'sub': case 'sup': case 'deg': case 'fName': case 'lim': tex = join(); break
    case 'sSup': tex = `{${part('e')}}^{${part('sup')}}`; break
    case 'sSub': tex = `{${part('e')}}_{${part('sub')}}`; break
    case 'sSubSup': tex = `{${part('e')}}_{${part('sub')}}^{${part('sup')}}`; break
    case 'sPre': tex = `{}_{${part('sub')}}^{${part('sup')}}{${part('e')}}`; break
    case 'f': {
      const type = value(first(node, 'fPr'), 'type', 'bar')
      tex = type === 'noBar'
        ? `\\genfrac{}{}{0pt}{}{${part('num')}}{${part('den')}}`
        : `\\frac{${part('num')}}{${part('den')}}`
      break
    }
    case 'rad': {
      const degree = part('deg')
      tex = `\\sqrt${degree && !truth(first(node, 'radPr'), 'degHide') ? `[${degree}]` : ''}{${part('e')}}`
      break
    }
    case 'd': {
      tex = delimitedFormula(node, render)
      break
    }
    case 'eqArr': tex = `\\begin{aligned}${elements(node).filter(child => child.localName === 'e').map(render).join(' \\\\ ')}\\end{aligned}`; break
    case 'm': {
      const rows = elements(node).filter(child => child.localName === 'mr')
      tex = `\\begin{matrix}${rows.map(row => elements(row).filter(cell => cell.localName === 'e').map(render).join(' & ')).join(' \\\\ ')}\\end{matrix}`
      break
    }
    case 'func': {
      const name = part('fName').replace(/\\ /g, '').replace(/^\\mathrm\{([a-z]+)\}$/, '$1')
      tex = `${/^(sin|cos|tan|cot|sec|csc|log|ln|exp|max|min|lim)$/.test(name) ? `\\${name}` : `\\operatorname{${name}}`} ${part('e')}`
      break
    }
    case 'nary': {
      const properties = first(node, 'naryPr')
      const symbol = value(properties, 'chr', '∫')
      const operator = { '∑': '\\sum', '∏': '\\prod', '∫': '\\int', '∬': '\\iint', '∭': '\\iiint', '∮': '\\oint', '⋃': '\\bigcup', '⋂': '\\bigcap', '⋁': '\\bigvee', '⋀': '\\bigwedge' }[symbol]
      if (!operator) throw new EquationError('公式含暂不支持的求和或积分运算符')
      const limits = value(properties, 'limLoc') === 'undOvr' ? '\\limits' : '\\nolimits'
      tex = operator + limits
      if (!truth(properties, 'subHide') && part('sub')) tex += `_{${part('sub')}}`
      if (!truth(properties, 'supHide') && part('sup')) tex += `^{${part('sup')}}`
      tex += `{${part('e')}}`
      break
    }
    case 'limLow': tex = `\\underset{${part('lim')}}{${part('e')}}`; break
    case 'limUpp': tex = `\\overset{${part('lim')}}{${part('e')}}`; break
    case 'bar': tex = `\\${value(first(node, 'barPr'), 'pos', 'top') === 'bot' ? 'underline' : 'overline'}{${part('e')}}`; break
    case 'acc': {
      const accent = value(first(node, 'accPr'), 'chr', '̂')
      const command = { '̂': 'hat', 'ˆ': 'hat', '^': 'hat', '̃': 'tilde', '~': 'tilde', '→': 'vec', '⃗': 'vec', '¯': 'bar', '̄': 'bar', '̇': 'dot', '̈': 'ddot', '˙': 'dot', '¨': 'ddot', '̌': 'check', '̆': 'breve' }[accent]
      if (!command) throw new EquationError('公式含暂不支持的重音符号')
      tex = `\\${command}{${part('e')}}`
      break
    }
    case 'groupChr': {
      const properties = first(node, 'groupChrPr')
      const symbol = value(properties, 'chr', '⏟')
      if (!['⏟', '⏞'].includes(symbol)) throw new EquationError('公式含暂不支持的组合符号')
      tex = `\\${value(properties, 'pos', 'bot') === 'bot' ? 'underbrace' : 'overbrace'}{${part('e')}}`
      break
    }
    case 'box': tex = part('e'); break
    case 'borderBox': tex = `\\boxed{${part('e')}}`; break
    case 'phant': throw new EquationError('公式含暂不支持的隐藏占位结构')
    default: throw new EquationError(`公式含暂不支持的结构（${kind.slice(0, 40)}）`)
  }
  if (tex.length > MAX_TEX) throw new EquationError('公式长度超过限制')
  return tex
}

function equationImage(tex, document) {
  const container = document.convert(tex, { display: false, em: 16, ex: 8, containerWidth: 1600 })
  const root = adaptor.firstChild(container)
  const xml = adaptor.outerHTML(root)
  if (xml.length > 1024 * 1024 || /data-mml-node="merror"|<merror\b/.test(xml)) throw new EquationError('公式绘制失败，请核对原公式')
  const svg = parseXml(xml).documentElement
  const viewBox = (svg.getAttribute('viewBox') || '').split(/\s+/).map(Number)
  if (viewBox.length !== 4 || !viewBox.every(Number.isFinite) || viewBox[2] <= 0 || viewBox[3] <= 0) throw new EquationError('公式绘制尺寸无效')
  return { svg, viewBox }
}

function equationRun(document, host, formula) {
  const run = document.createElementNS(A, 'a:r')
  // Office equations often carry font size/color inside a math run. Otherwise
  // inherit the preceding ordinary run or paragraph defaults, like PowerPoint.
  const own = descendants(host, A, 'rPr')[0]
  let previous = host.previousSibling
  while (previous && previous.nodeType !== 1) previous = previous.previousSibling
  const nearby = previous && descendants(previous, A, 'rPr')[0]
  const properties = (own || nearby)?.cloneNode(true) || document.createElementNS(A, 'a:rPr')
  // One unique family keeps the placeholder atomic and supplies its true
  // ascent/descent even when the slide engine probes the font with "Mg".
  for (const name of ['latin', 'ea', 'cs', 'sym']) {
    for (const old of elements(properties).filter(child => child.localName === name)) properties.removeChild(old)
    const font = document.createElementNS(A, `a:${name}`)
    font.setAttribute('typeface', formula.family)
    properties.appendChild(font)
  }
  for (const link of elements(properties).filter(child => ['hlinkClick', 'hlinkMouseOver'].includes(child.localName))) properties.removeChild(link)
  run.appendChild(properties)
  const text = document.createElementNS(A, 'a:t')
  text.textContent = formula.marker
  run.appendChild(text)
  return run
}

function flattenMathAlternates(document) {
  // A compatibility fallback is another representation of the same equation,
  // not an additional equation or picture to paint a second time.
  for (const alternate of descendants(document, MC, 'AlternateContent').reverse()) {
    const choice = elements(alternate).find(child => child.localName === 'Choice' && descendants(child, M, 'oMath').length)
    if (!choice) continue
    for (const child of Array.from(choice.childNodes)) alternate.parentNode.insertBefore(child, alternate)
    alternate.parentNode.removeChild(alternate)
  }
}

function slidePageNumbers(parts) {
  const presentation = parts['ppt/presentation.xml']
  const relationships = parts['ppt/_rels/presentation.xml.rels']
  const pages = new Map()
  if (!presentation || !relationships) return pages
  const targets = new Map(descendants(parseXml(strFromU8(relationships)), PKG_R, 'Relationship')
    .filter(node => node.getAttribute('TargetMode') !== 'External' && node.getAttribute('Type')?.endsWith('/slide'))
    .map(node => {
      const target = node.getAttribute('Target') || ''
      return [node.getAttribute('Id'), posix.normalize(target.startsWith('/') ? target.slice(1) : posix.join('ppt', target))]
    }))
  descendants(parseXml(strFromU8(presentation)), P, 'sldId').forEach((node, index) => {
    const name = targets.get(node.getAttributeNS(R, 'id'))
    if (name) pages.set(name, index + 1)
  })
  return pages
}

export function preparePptxMath(input) {
  let expanded = 0
  const xmlParts = unzipSync(input, { filter(entry) {
    expanded += entry.originalSize
    if (expanded > 256 * 1024 * 1024) throw new EquationError('PPTX 解压大小超过处理限制')
    return /^ppt\/(slides|slideLayouts|slideMasters)\/[^/]+\.xml$/.test(entry.name)
      || ['ppt/presentation.xml', 'ppt/_rels/presentation.xml.rels'].includes(entry.name)
  } })
  const sourceParts = Object.entries(xmlParts).map(([name, bytes]) => {
    if (bytes.length > MAX_XML) throw new EquationError('PPTX 页面 XML 超过处理限制')
    return [name, strFromU8(bytes)]
  })
  const usedMarkers = new Set(sourceParts.flatMap(([, xml]) => xml.match(/[\ue000-\uf8ff]/g) || []))
  let markerCode = 0xe000
  const formulas = new Map()
  const patches = new Map()
  const pageNumbers = slidePageNumbers(xmlParts)
  // Generated TeX cannot load extensions, URLs, HTML, or user-defined macros.
  const mathDocument = mathjax.document('', {
    InputJax: new TeX({ packages: ['base', 'ams'], maxBuffer: MAX_TEX, maxMacros: 1000,
      formatError() { throw new EquationError('公式转换失败，请核对原公式') },
    }),
    OutputJax: new SVG({ fontCache: 'none' }),
  })
  for (const [name, xml] of sourceParts) {
    if (!/<(?:[\w-]+:)?oMath(?:Para)?\b/.test(xml)) continue
    let equationIndex = 0
    try {
      const document = parseXml(xml)
      flattenMathAlternates(document)
      const roots = descendants(document, M, '*').filter(node => isEquation(node) && !isEquation(node.parentNode))
      for (const root of roots) {
        // A previous atom can have replaced their shared a14:m host already.
        if (!document.documentElement.contains(root)) continue
        equationIndex++
        let host = root
        while (host.parentNode && !(host.parentNode.namespaceURI === A && host.parentNode.localName === 'p')) host = host.parentNode
        if (!host.parentNode) throw new EquationError('公式未放在可绘制的幻灯片文本段落中')
        // Several oMath atoms may belong to one oMathPara/a14:m object.
        if (descendants(host, A, 't').length) throw new EquationError('公式与普通文本的混合结构暂不支持，请先导出 PDF')
        const equations = isEquation(host) ? [host] : descendants(host, M, '*').filter(node => isEquation(node) && !isEquation(node.parentNode))
        const tex = equations.map(node => ommlToLatex(node)).join('\\quad ')
        if (!tex.trim()) throw new EquationError('公式内容为空，无法保留原公式')
        if (formulas.size >= MAX_EQUATIONS) throw new EquationError('PPTX 公式数量超过处理限制')
        while (usedMarkers.has(String.fromCharCode(markerCode))) markerCode++
        if (markerCode > 0xf8ff) throw new EquationError('PPTX 特殊字符超过处理限制')
        const marker = String.fromCharCode(markerCode++)
        usedMarkers.add(marker)
        const formula = { marker, family: `KBFormula${formulas.size}`, tex, ...equationImage(tex, mathDocument) }
        formulas.set(marker, formula)
        host.parentNode.replaceChild(equationRun(document, host, formula), host)
      }
      patches.set(name, strToU8(serializer.serializeToString(document)))
    } catch (error) {
      if (!(error instanceof EquationError)) throw error
      const page = pageNumbers.get(name)
      const location = page ? `第 ${page} 页` : name.includes('/slideMasters/') ? '幻灯片母版' : name.includes('/slideLayouts/') ? '幻灯片版式' : '幻灯片'
      throw new EquationError(`${location}${equationIndex ? `第 ${equationIndex} 个公式` : ''}：${error.message}`)
    }
  }
  const familyMap = new Map([...formulas.values()].map(formula => [formula.family, formula]))
  const find = family => familyMap.get(/\bKBFormula\d+\b/.exec(family)?.[0])
  if (!patches.size) return { input, formulas, find }
  const archive = unzipSync(input)
  for (const [name, bytes] of patches) archive[name] = bytes
  return { input: zipSync(archive), formulas, find }
}

function metrics(formula, sizePx) {
  const [, y, width, height] = formula.viewBox
  const scale = sizePx / 1000 // MathJax SVG's viewBox uses 1000 units per em.
  return { widthPx: width * scale, ascentPx: Math.max(0, -y * scale), descentPx: Math.max(0, (height + y) * scale), lineGapPx: 0 }
}

export function equationMeasurer(math, ordinary) {
  return (text, spec) => {
    const formula = math.find(spec.family)
    return formula ? metrics(formula, spec.sizePx) : ordinary(text, spec)
  }
}

export function restoreMathText(text, math) {
  return text.replace(/[\ue000-\uf8ff]/g, marker => math.formulas.has(marker) ? `\\(${math.formulas.get(marker).tex}\\)` : marker)
}

export function paintMathSvg(svg, math, measureText) {
  if (!math.formulas.size) return svg
  const document = parseXml(svg)
  for (const text of descendants(document, S, 'text')) {
    const spans = descendants(text, S, 'tspan')
    if (!spans.some(span => math.find(span.getAttribute('font-family') || ''))) continue
    const x = Number(text.getAttribute('x')), y = Number(text.getAttribute('y'))
    if (!Number.isFinite(x) || !Number.isFinite(y)) throw new EquationError('公式在幻灯片中的位置无效')
    const pieces = spans.map(span => {
      const size = Number(span.getAttribute('font-size') || 16)
      const family = span.getAttribute('font-family') || ''
      const formula = math.find(family)
      const dx = Number(span.getAttribute('dx') || 0)
      const spec = { family, sizePx: size, bold: Number(span.getAttribute('font-weight')) >= 600, italic: span.getAttribute('font-style') === 'italic', letterSpacingPx: Number(span.getAttribute('letter-spacing') || 0), kerning: span.getAttribute('font-kerning') !== 'none' }
      const width = formula ? metrics(formula, size).widthPx : measureText(span.textContent || '', spec).widthPx
      if (![size, dx, width].every(Number.isFinite) || size <= 0 || width < 0) throw new EquationError('公式文本尺寸无效')
      return { span, formula, size, width, dx }
    })
    const width = pieces.reduce((sum, piece) => sum + piece.width + piece.dx, 0)
    const anchor = text.getAttribute('text-anchor')
    let cursor = x - (anchor === 'middle' ? width / 2 : anchor === 'end' ? width : 0)
    const group = document.createElementNS(S, 'g')
    // Effects applied to the line must apply to equations too.
    for (const name of ['filter', 'transform', 'opacity', 'clip-path', 'mask']) {
      if (text.hasAttribute(name)) { group.setAttribute(name, text.getAttribute(name)); text.removeAttribute(name) }
    }
    text.parentNode.replaceChild(group, text)
    group.appendChild(text)
    text.setAttribute('text-anchor', 'start')
    for (const { span, formula, size, width, dx } of pieces) {
      cursor += dx
      span.setAttribute('x', String(cursor))
      span.removeAttribute('dx')
      if (formula) {
        const image = document.importNode(formula.svg, true)
        const [vx, vy, vw, vh] = formula.viewBox
        const scale = size / 1000
        const shift = Number(span.getAttribute('baseline-shift') || 0)
        image.setAttribute('x', String(cursor + vx * scale))
        image.setAttribute('y', String(y - shift + vy * scale))
        image.setAttribute('width', String(vw * scale))
        image.setAttribute('height', String(vh * scale))
        image.setAttribute('color', span.getAttribute('fill') || '#000000')
        image.setAttribute('data-knowledge-equation', formula.tex)
        image.removeAttribute('style')
        group.appendChild(image)
        span.textContent = ''
      }
      cursor += width
    }
  }
  const result = serializer.serializeToString(document)
  if (/[\ue000-\uf8ff]/.test(descendants(document, S, 'text').map(node => node.textContent).join(''))) {
    // Only reject our reserved objects; real private-use glyphs can be valid.
    for (const marker of math.formulas.keys()) if (result.includes(marker)) throw new EquationError('部分公式未能绘制，已停止导入以避免内容缺失')
  }
  return result
}
