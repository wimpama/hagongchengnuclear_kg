// Code-only PPTX parsing and rasterization. No Office, browser or network.
import { existsSync } from 'node:fs'
import { readFile, writeFile, mkdir } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { Resvg } from '@resvg/resvg-js'
import {
  getSlides, getSlideSize, getSlideText, getSlideXmlString, loadPresentation,
} from '@office-kit/pptx'
import {
  FONT_FILES, SANS, SERIF, MONO, buildFontkitMeasurer, renderSlideToSvg,
} from '@office-kit/pptx-preview/node'

const MAX_BYTES = 100 * 1024 * 1024
const MAX_IMAGE = 10 * 1024 * 1024

export class DocumentError extends Error {}

function fonts() {
  const candidates = [
    process.env.KB_DOCUMENT_FONT_PATH,
    'C:/Windows/Fonts/simhei.ttf',
    '/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf',
    '/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.otf',
  ].filter(Boolean)
  const file = candidates.find(candidate => existsSync(candidate))
  // Fail clearly instead of indexing pictures with missing Chinese glyphs.
  if (!file) throw new DocumentError('缺少中文字体，请配置 KB_DOCUMENT_FONT_PATH 为支持中文的 TTF/OTF 字体；后端镜像会自动提供')
  return { file, measureText: buildFontkitMeasurer({ fonts: [{ family: 'Knowledge CJK', source: file }] }) }
}

export function checkedSlideSvg(pres, slide, page, measureText) {
  const xml = getSlideXmlString(slide)
  if (/<(?:[\w-]+:)?(?:oMath|oMathPara)\b/.test(xml)) {
    throw new DocumentError(`第 ${page} 页含当前代码绘图库不支持的 Office 数学公式，请先导出为 PDF 后导入，以保留原公式`)
  }
  const svg = renderSlideToSvg(pres, slide, { textLayout: 'svg', measureText })
  const fallbacks = [...new Set([...svg.matchAll(/data-pptx-fallback=["']([^"']+)["']/g)].map(match => match[1]))]
  if (fallbacks.length) {
    throw new DocumentError(`第 ${page} 页存在暂不支持的图形（${fallbacks.join('、')}），请先导出为 PDF 后导入；未忽略这些图形`)
  }
  if (/(?:href|xlink:href)=["'](?!data:|#)[^"']+/i.test(svg)) {
    throw new DocumentError(`第 ${page} 页含外部图片引用，请将图片嵌入课件后重试`)
  }
  return svg
}

export async function renderPptx(source, output, maxPages = 200) {
  const input = await readFile(source)
  if (!input.length || input.length > MAX_BYTES) throw new DocumentError('PPTX 文件为空或超过 100MB')
  const pres = await loadPresentation(input)
  const slides = getSlides(pres)
  if (!slides.length || slides.length > maxPages) throw new DocumentError(`PPTX 页数必须在 1 到 ${maxPages} 页之间，请拆分资料`)
  const size = getSlideSize(pres)
  const aspect = size && size.width / size.height
  if (!Number.isFinite(aspect) || aspect < 0.2 || aspect > 5) throw new DocumentError('PPTX 幻灯片尺寸无效或超出支持范围')
  const { file, measureText } = fonts()
  await mkdir(output, { recursive: true })
  const pages = []
  let total = 0
  for (const [index, slide] of slides.entries()) {
    const page = index + 1
    const text = getSlideText(slide).trim()
    const svg = checkedSlideSvg(pres, slide, page, measureText)
    const png = new Resvg(svg, {
      fitTo: { mode: aspect >= 1 ? 'width' : 'height', value: 1600 },
      font: { fontFiles: [...FONT_FILES, file], loadSystemFonts: false, defaultFontFamily: SANS, sansSerifFamily: SANS, serifFamily: SERIF, monospaceFamily: MONO },
    }).render().asPng()
    total += png.length
    if (png.length > MAX_IMAGE || total > MAX_BYTES) throw new DocumentError('幻灯片图片超过处理限制，请拆分后上传')
    const imageFile = `${page}.png`
    await writeFile(path.join(output, imageFile), png)
    pages.push({ page, text, imageFile })
  }
  return { pages }
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const limit = Number(process.argv[4] || 200)
    if (!Number.isInteger(limit) || limit < 1 || limit > 500) throw new DocumentError('文档页数配置无效')
    const result = await renderPptx(process.argv[2], process.argv[3], limit)
    process.stdout.write(JSON.stringify(result))
  } catch (error) {
    if (error instanceof DocumentError) process.stdout.write(JSON.stringify({ error: error.message }))
    else {
      // Never expose source XML, file paths or embedded data in diagnostics.
      process.stdout.write(JSON.stringify({ error: 'PPTX 代码解析失败，请检查文件格式、字体与处理依赖，或导出为 PDF 后重试' }))
      process.exitCode = 0
    }
  }
}
