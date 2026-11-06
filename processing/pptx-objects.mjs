// Recover the preview that Office already saved for an embedded object. Never
// execute OLE, fetch a link, or infer a replacement for a scientific diagram.
import { DOMParser, XMLSerializer } from '@xmldom/xmldom'
import { unzipSync, zipSync, strFromU8, strToU8 } from 'fflate'
import { posix } from 'node:path'
import { slidePageNumbers } from './pptx-math.mjs'

const P = 'http://schemas.openxmlformats.org/presentationml/2006/main'
const A = 'http://schemas.openxmlformats.org/drawingml/2006/main'
const R = 'http://schemas.openxmlformats.org/officeDocument/2006/relationships'
const MC = 'http://schemas.openxmlformats.org/markup-compatibility/2006'
const PKG_R = 'http://schemas.openxmlformats.org/package/2006/relationships'
const serializer = new XMLSerializer()
const children = node => Array.from(node?.childNodes || []).filter(child => child.nodeType === 1)
const all = (node, ns, name) => Array.from(node.getElementsByTagNameNS(ns, name))
const first = (node, ns, name) => children(node).find(child => child.namespaceURI === ns && child.localName === name)

export class ObjectPreviewError extends Error {}
function parse(xml) {
  if (xml.length > 20 * 1024 * 1024 || /<!DOCTYPE|<!ENTITY/i.test(xml)) throw new ObjectPreviewError('PPTX 图形 XML 超出限制或含不支持的声明')
  return new DOMParser({ onError() { throw new ObjectPreviewError('PPTX 图形 XML 无效') } }).parseFromString(xml, 'application/xml')
}
function name(frame) {
  const text = all(frame, P, 'cNvPr')[0]?.getAttribute('name') || ''
  return text ? `「${text.replace(/[\p{Cc}\p{Cf}]/gu, '').slice(0, 64)}」` : ''
}
function kind(frame) {
  const data = all(frame, A, 'graphicData')[0]
  if (all(frame, P, 'oleObj').length) return 'OLE 嵌入对象'
  const uri = data?.getAttribute('uri') || ''
  if (uri.endsWith('/diagram')) return 'SmartArt 图示'
  if (uri.endsWith('/chart')) return '图表'
  if (uri.endsWith('/table')) return '表格'
  return '图形对象'
}
export function describeGraphics(xml) {
  return [...new Set(all(parse(xml), P, 'graphicFrame').map(frame => `${kind(frame)}${name(frame)}`))].join('、')
}

function flattenAlternates(document) {
  // Only slide-level graphical alternatives. Inline equation alternatives are
  // handled separately by the OMML adapter, with their text layout intact.
  for (const alternate of all(document, MC, 'AlternateContent')) {
    if (alternate.parentNode?.namespaceURI !== P || !['spTree', 'grpSp'].includes(alternate.parentNode.localName)) continue
    const branches = children(alternate)
    const usable = branch => children(branch).length > 0 && children(branch).every(node => node.namespaceURI === P && ['sp', 'pic', 'grpSp', 'cxnSp', 'graphicFrame'].includes(node.localName))
    const choice = branches.find(branch => branch.localName === 'Choice' && usable(branch))
    const fallback = branches.find(branch => branch.localName === 'Fallback' && usable(branch))
    // A saved picture/group fallback is more faithful than a placeholder for
    // a container the renderer does not implement (e.g. legacy OLE/SmartArt).
    const unsupported = choice && all(choice, P, 'graphicFrame').some(frame => !['图表', '表格'].includes(kind(frame)))
    const selected = unsupported && fallback ? fallback : choice || fallback
    if (!selected) throw new ObjectPreviewError('图形兼容分支没有可用的内嵌预览，请导出为 PDF 后导入')
    for (const child of children(selected)) alternate.parentNode.insertBefore(child, alternate)
    alternate.parentNode.removeChild(alternate)
  }
}

export function preparePptxObjects(input) {
  let expanded = 0, entries = 0
  const parts = unzipSync(input, { filter(entry) {
    expanded += entry.originalSize
    if (++entries > 10000 || expanded > 256 * 1024 * 1024) throw new ObjectPreviewError('PPTX 解压大小超过处理限制')
    return true
  } })
  const pages = slidePageNumbers(parts)
  let changed = false, recovered = 0
  for (const [part, bytes] of Object.entries(parts)) {
    if (!/^ppt\/(slides|slideLayouts|slideMasters)\/[^/]+\.xml$/.test(part)) continue
    const xml = strFromU8(bytes)
    if (!/graphicFrame|AlternateContent/.test(xml)) continue
    const document = parse(xml)
    try {
      flattenAlternates(document)
      const relPath = posix.join(posix.dirname(part), '_rels', `${posix.basename(part)}.rels`)
      const rels = parts[relPath] ? all(parse(strFromU8(parts[relPath])), PKG_R, 'Relationship') : []
      for (const frame of all(document, P, 'graphicFrame')) {
        const object = all(frame, P, 'oleObj')[0]
        if (!object) continue // Native charts and tables continue through the renderer.
        const label = `${kind(frame)}${name(frame)}`
        const picture = all(object, P, 'pic')[0]
        if (!picture || ['1', 'true'].includes(object.getAttribute('showAsIcon'))) {
          throw new ObjectPreviewError(`${label}没有可用的内容预览图，请导出为 PDF 后导入；未忽略该对象`)
        }
        const blip = all(picture, A, 'blip')[0]
        const edge = rels.find(rel => rel.getAttribute('Id') === blip?.getAttributeNS(R, 'embed'))
        const target = edge?.getAttribute('Target') || ''
        const media = posix.normalize(target.startsWith('/') ? target.slice(1) : posix.join(posix.dirname(part), target))
        if (!target || edge.getAttribute('TargetMode') === 'External' || !edge.getAttribute('Type')?.endsWith('/image') || !parts[media]) {
          throw new ObjectPreviewError(`${label}的预览图未嵌入或已损坏，请导出为 PDF 后导入；未忽略该对象`)
        }
        const preview = picture.cloneNode(true)
        const spPr = first(preview, P, 'spPr')
        const frameTransform = first(frame, P, 'xfrm')
        if (!spPr || !frameTransform) throw new ObjectPreviewError(`${label}缺少预览位置或尺寸，请导出为 PDF 后导入`)
        const transform = document.createElementNS(A, 'a:xfrm')
        for (const attr of Array.from(frameTransform.attributes)) transform.setAttribute(attr.name, attr.value)
        for (const child of children(frameTransform)) transform.appendChild(child.cloneNode(true))
        const old = first(spPr, A, 'xfrm')
        if (old) spPr.replaceChild(transform, old)
        else spPr.insertBefore(transform, spPr.firstChild)
        const frameName = all(frame, P, 'cNvPr')[0]
        const previewName = all(preview, P, 'cNvPr')[0]
        if (frameName && previewName) previewName.parentNode.replaceChild(frameName.cloneNode(true), previewName)
        frame.parentNode.replaceChild(preview, frame)
        recovered++
      }
    } catch (error) {
      if (error instanceof ObjectPreviewError) {
        const location = pages.has(part) ? `第 ${pages.get(part)} 页` : part.includes('slideMasters') ? '幻灯片母版' : '幻灯片版式'
        throw new ObjectPreviewError(`${location}${error.message}`)
      }
      throw error
    }
    const patched = serializer.serializeToString(document)
    if (patched !== xml) { parts[part] = strToU8(patched); changed = true }
  }
  return { input: changed ? zipSync(parts) : input, recovered }
}
