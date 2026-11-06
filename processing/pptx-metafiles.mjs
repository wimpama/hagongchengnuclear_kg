// Decode the vector previews already embedded in Office files. OLE programs
// are never opened. The resulting PNGs exist only in the rendering copy.
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { posix } from 'node:path'
import { DOMParser, XMLSerializer } from '@xmldom/xmldom'
import { strFromU8, strToU8 } from 'fflate'
import { convertMetafileToSvg, getMetafileInfo } from 'emf-converter'
import { Resvg } from '@resvg/resvg-js'
import { create as createFont } from 'fontkit'
import { FONT_FILES, SANS, SERIF, MONO, TIMES, ARIAL, substituteFamily } from '@office-kit/pptx-preview/node'

const SVG = 'http://www.w3.org/2000/svg'
const REL = 'http://schemas.openxmlformats.org/package/2006/relationships'
const CT = 'http://schemas.openxmlformats.org/package/2006/content-types'
const MATH_FONT = fileURLToPath(new URL('./fonts/STIXTwoMath-Regular.otf', import.meta.url))
const symbolTable = new Map()
for (const line of readFileSync(new URL('./fonts/symbol.txt', import.meta.url), 'utf8').split('\n')) {
  const match = line.match(/^([\da-f]+)\s+([\da-f]+)\s+#/i)
  if (match) symbolTable.set(parseInt(match[2], 16), parseInt(match[1], 16))
}
// The old Adobe table predates Unicode's public bracket-piece assignments.
const publicSymbols = new Map(Object.entries({
  F8E5: 0x23af, F8E6: 0x23d0, F8E7: 0x23af,
  F6DA: 0xae, F6D9: 0xa9, F6DB: 0x2122, F8E8: 0xae, F8E9: 0xa9, F8EA: 0x2122,
  F8EB: 0x239b, F8EC: 0x239c, F8ED: 0x239d,
  F8EE: 0x23a1, F8EF: 0x23a2, F8F0: 0x23a3,
  F8F1: 0x23a7, F8F2: 0x23a8, F8F3: 0x23a9, F8F4: 0x23aa,
  F8F5: 0x23ae, F8F6: 0x239e, F8F7: 0x239f, F8F8: 0x23a0,
  F8F9: 0x23a4, F8FA: 0x23a5, F8FB: 0x23a6,
  F8FC: 0x23ab, F8FD: 0x23ac, F8FE: 0x23ad,
}).map(([code, value]) => [parseInt(code, 16), value]))
const serializer = new XMLSerializer()
const all = (node, ns, name) => Array.from(node.getElementsByTagNameNS(ns, name))
const isPng = bytes => bytes?.length >= 8 && [137, 80, 78, 71, 13, 10, 26, 10].every((value, index) => bytes[index] === value)
const fontCache = new Map()

function fonts(fontFile) {
  const key = fontFile || ''
  if (fontCache.has(key)) return fontCache.get(key)
  const files = [...FONT_FILES, ...(fontFile ? [fontFile] : [])]
  const buffers = files.map(file => readFileSync(file))
  const cjk = fontFile ? createFont(buffers.at(-1)) : null
  const family = cjk?.familyName || cjk?.fonts?.[0]?.familyName || SANS
  const result = { files, buffers, family }
  fontCache.set(key, result)
  return result
}

export class MetafileError extends Error {}
function parse(xml) {
  if (xml.length > 20 * 1024 * 1024 || /<!DOCTYPE|<!ENTITY/i.test(xml)) throw new MetafileError('矢量预览 XML 超出限制或含不支持的声明')
  return new DOMParser({ onError() { throw new MetafileError('矢量预览 XML 无效') } }).parseFromString(xml, 'application/xml')
}

function normalizeWmfFonts(input) {
  // Some MathType WMFs mark Symbol as ANSI/DEFAULT_CHARSET. Windows still
  // uses the Symbol face's one-byte encoding; GBK decoding corrupts arrows.
  const bytes = Uint8Array.from(input)
  const view = new DataView(bytes.buffer)
  if (bytes.length < 18) throw new MetafileError('WMF 预览已损坏')
  let offset = view.getUint32(0, true) === 0x9ac6cdd7 ? 22 : 0
  if (offset + 18 > bytes.length || view.getUint16(offset + 2, true) !== 9) throw new MetafileError('WMF 预览头无效')
  offset += 18
  let records = 0, ended = false
  while (offset + 6 <= bytes.length) {
    const length = view.getUint32(offset, true) * 2
    const fn = view.getUint16(offset + 4, true)
    if (++records > 200000 || length < 6 || offset + length > bytes.length) throw new MetafileError('WMF 预览记录已损坏或超过限制')
    if (fn === 0x2fb && length >= 25) {
      const data = offset + 6
      const face = String.fromCharCode(...bytes.subarray(data + 18, Math.min(data + 50, offset + length))).split('\0')[0]
      if (face.trim().toLowerCase() === 'symbol') bytes[data + 13] = 2
      // The face name follows the author's Windows ANSI page, while the
      // record's charset controls its text. Keep Latin text on CP1252.
      const chinese = new TextDecoder('gbk').decode(bytes.subarray(data + 18, Math.min(data + 50, offset + length))).split('\0')[0]
      const alias = { 宋体: 'SimSun', 新宋体: 'NSimSun', 黑体: 'SimHei' }[chinese]
      if (alias && offset + length - data - 18 >= alias.length + 1) {
        bytes.fill(0, data + 18, Math.min(data + 50, offset + length))
        bytes.set(Buffer.from(alias, 'ascii'), data + 18)
      }
    }
    offset += length
    if (fn === 0) { ended = true; break }
  }
  if (!ended) throw new MetafileError('WMF 预览记录不完整')
  return bytes
}

function normalizeText(svg, cjkFamily) {
  const document = parse(svg)
  for (const node of all(document, SVG, 'text')) {
    const family = (node.getAttribute('font-family') || '').replaceAll('"', '').trim()
    if (family.toLowerCase() === 'symbol') {
      node.textContent = [...node.textContent].map(char => {
        const code = char.codePointAt(0)
        // Some EMF producers already store Unicode. WMF characters remain
        // one byte; translate only the legacy Symbol encoding there.
        const mapped = code <= 0xff ? symbolTable.get(code) : code
        if (mapped === undefined) throw new MetafileError('Symbol 公式包含无法识别的字符')
        return String.fromCodePoint(publicSymbols.get(mapped) ?? mapped)
      }).join('')
      node.setAttribute('font-family', 'STIX Two Math')
    } else {
      node.setAttribute('font-family', /宋体|黑体|simsun|simhei|nsimsun|sung|hei/i.test(family) || /\p{Script=Han}/u.test(node.textContent) ? cjkFamily : substituteFamily(family))
    }
  }
  // Only data images and local SVG fragments may survive conversion.
  for (const node of all(document, SVG, '*')) {
    if (['script', 'foreignObject'].includes(node.localName)) throw new MetafileError('矢量预览含不支持的内容')
    for (const attr of Array.from(node.attributes)) {
      if (/^on/i.test(attr.name) || (attr.localName === 'href' && !/^(?:data:image\/(?:png|jpeg|gif|webp|bmp);base64,|#)/i.test(attr.value))) throw new MetafileError('矢量预览含外部资源')
    }
  }
  return serializer.serializeToString(document)
}

export async function metafileSvg(input, { format = 'wmf', fontFile } = {}) {
  if (!input.length || input.length > 10 * 1024 * 1024) throw new MetafileError('矢量预览为空或超过 10MiB')
  const bytes = format === 'wmf' ? normalizeWmfFonts(input) : input
  const options = { ansiCodePage: 1252, maxWidth: 1600, maxHeight: 1600, maxCanvasDimension: 1600, maxRecords: 200000, timeoutMs: 10000 }
  // WMF record bounds/EOF were checked above. EMF inspection also validates
  // the nested EMF+ stream before drawing. No unbounded font-directory scan.
  if (format !== 'wmf') {
    const info = await getMetafileInfo(bytes, options)
    if (!info || info.truncated || Object.keys(info.recordTypes).some(type => type.startsWith('UNKNOWN_'))) throw new MetafileError('矢量预览已损坏或包含不支持的记录')
    const font = fonts(fontFile)
    options.fonts = font.buffers
    options.fontFamilyMap = { 'times new roman': TIMES, arial: ARIAL, 宋体: font.family, 新宋体: font.family, simsun: font.family, nsimsun: font.family, simhei: font.family }
  }
  const svg = await convertMetafileToSvg(bytes, options)
  if (!svg || svg.length > 20 * 1024 * 1024) throw new MetafileError('矢量预览无法完整解析')
  return normalizeText(svg, fonts(fontFile).family)
}

export async function metafilePng(input, options = {}) {
  const normalized = await metafileSvg(input, options)
  const root = parse(normalized).documentElement
  const longest = Math.max(Number(root.getAttribute('width')), Number(root.getAttribute('height')))
  if (!Number.isFinite(longest) || longest <= 0) throw new MetafileError('矢量预览尺寸无效')
  const png = new Resvg(normalized, {
    fitTo: { mode: 'zoom', value: Math.min(4, 1600 / longest) },
    font: { fontFiles: [...fonts(options.fontFile).files, MATH_FONT], loadSystemFonts: false, defaultFontFamily: SERIF, sansSerifFamily: SANS, serifFamily: SERIF, monospaceFamily: MONO },
  }).render().asPng()
  if (png.length > 10 * 1024 * 1024) throw new MetafileError('矢量预览生成的图片超过限制')
  return png
}

export async function prepareMetafileImages(parts, pages, options) {
  const media = new Map()
  for (const [part, bytes] of Object.entries(parts)) {
    if (!/^ppt\/(slides|slideLayouts|slideMasters)\/_rels\/[^/]+\.xml\.rels$/.test(part)) continue
    const owner = posix.join(posix.dirname(posix.dirname(part)), posix.basename(part).slice(0, -5))
    if (!parts[owner]) continue
    const drawing = parse(strFromU8(parts[owner]))
    const references = new Set(all(drawing, '*', '*').map(node => node.getAttributeNS('http://schemas.openxmlformats.org/officeDocument/2006/relationships', 'embed')).filter(Boolean))
    for (const rel of all(parse(strFromU8(bytes)), REL, 'Relationship')) {
      if (!references.has(rel.getAttribute('Id')) || rel.getAttribute('TargetMode') === 'External' || !rel.getAttribute('Type')?.endsWith('/image')) continue
      const target = rel.getAttribute('Target') || ''
      const path = posix.normalize(target.startsWith('/') ? target.slice(1) : posix.join(posix.dirname(owner), target))
      if (!/\.(wmf|emf)$/i.test(path) || media.has(path)) continue
      // A rendering copy may already have a PNG override at this part name.
      if (isPng(parts[path])) continue
      media.set(path, pages.has(owner) ? `第 ${pages.get(owner)} 页` : owner.includes('slideMasters') ? '幻灯片母版' : '幻灯片版式')
    }
  }
  if (!media.size) return 0
  const types = parse(strFromU8(parts['[Content_Types].xml']))
  let total = 0, expanded = Object.values(parts).reduce((sum, bytes) => sum + bytes.length, 0)
  for (const [path, location] of media) {
    try {
      if (!parts[path]) throw new MetafileError('矢量预览未嵌入')
      const png = await metafilePng(parts[path], { ...options, format: path.toLowerCase().endsWith('.wmf') ? 'wmf' : 'emf' })
      total += png.length
      if (total > 100 * 1024 * 1024) throw new MetafileError('矢量预览图片合计超过 100MiB')
      expanded += png.length - parts[path].length
      if (expanded > 256 * 1024 * 1024) throw new MetafileError('PPTX 预览副本超过 256MiB')
      parts[path] = png
      // OPC content types are authoritative even when a part keeps its name.
      let override = all(types, CT, 'Override').find(node => node.getAttribute('PartName') === `/${path}`)
      if (!override) { override = types.createElementNS(CT, 'Override'); types.documentElement.appendChild(override) }
      override.setAttribute('PartName', `/${path}`)
      override.setAttribute('ContentType', 'image/png')
    } catch (error) {
      if (error instanceof MetafileError) throw new MetafileError(`${location}${error.message}；未忽略该预览`)
      throw new MetafileError(`${location}WMF/EMF 预览无法还原，请导出为 PDF 后导入；未忽略该预览`)
    }
  }
  parts['[Content_Types].xml'] = strToU8(serializer.serializeToString(types))
  return media.size
}
