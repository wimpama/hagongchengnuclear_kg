import { writeFile } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { Resvg } from '@resvg/resvg-js'
import { unzipSync, zipSync, strFromU8, strToU8 } from 'fflate'
import {
  createPresentation, addSlide, addSlideTextBox, addSlideImage, addSlideChart,
  getSlideLayouts, inches, moveSlide, savePresentation,
} from '@office-kit/pptx'

export async function createFixture() {
  const pres = createPresentation()
  const layout = getSlideLayouts(pres)[0]
  const text = addSlide(pres, { layout })
  addSlideTextBox(text, { x: inches(1), y: inches(0.5), w: inches(8), h: inches(1), text: '核反应截面 NEUTRON CROSS SECTION' })
  const picture = new Resvg('<svg xmlns="http://www.w3.org/2000/svg" width="240" height="120"><rect width="240" height="120" fill="#ed1515"/></svg>').render().asPng()
  addSlideImage(text, picture, { x: inches(1), y: inches(2), w: inches(4), h: inches(2), format: 'png' })
  const chart = addSlide(pres, { layout })
  addSlideTextBox(chart, { x: inches(1), y: inches(0.3), w: inches(8), h: inches(1), text: 'CHART PAGE 能量与截面' })
  addSlideChart(chart, { x: inches(1), y: inches(1.5), w: inches(8), h: inches(3), spec: { kind: 'line', categories: ['1', '2', '3'], series: [{ name: '截面', values: [10, 20, 15] }], title: 'COURSE CHART' } })
  const imageOnly = addSlide(pres, { layout })
  addSlideImage(imageOnly, picture, { x: inches(1), y: inches(1), w: inches(8), h: inches(4), format: 'png' })
  // Actual presentation order differs from the slideN.xml numbering.
  moveSlide(pres, imageOnly, 0)
  return savePresentation(pres)
}

const mathRun = text => `<m:r><m:t>${text}</m:t></m:r>`
const power = (base, exponent) => `<m:sSup><m:e>${mathRun(base)}</m:e><m:sup>${mathRun(exponent)}</m:sup></m:sSup>`

// Chinese prose and native Office equations similar to the reported slide.
// Red belongs only to equations, so raster tests can detect missing formulas.
export async function createMathFixture() {
  const pres = createPresentation()
  const layout = getSlideLayouts(pres)[0]
  const slide = addSlide(pres, { layout })
  const rows = [
    ['中子寿命：', `${power('10', '−4')}${mathRun('~')}${power('10', '−3')}`, ' s'],
    ['中子密度：', `<m:f><m:num>${power('10', '16')}</m:num><m:den>${power('m', '3')}</m:den></m:f>`, '，随入射能量变化'],
    ['截面符号：', `<m:sSub><m:e>${mathRun('σ')}</m:e><m:sub>${mathRun('s')}</m:sub></m:sSub>${mathRun('=')}<m:rad><m:deg/><m:e>${mathRun('E')}</m:e></m:rad>`, ''],
    ['积分范围：', `<m:nary><m:naryPr><m:chr m:val="∫"/><m:limLoc m:val="subSup"/></m:naryPr><m:sub>${mathRun('0')}</m:sub><m:sup>${mathRun('∞')}</m:sup><m:e>${mathRun('f(E)dE')}</m:e></m:nary>`, ''],
    ['散射矩阵：', `<m:d><m:dPr><m:begChr m:val="["/><m:endChr m:val="]"/></m:dPr><m:e><m:m><m:mr><m:e>${mathRun('1')}</m:e><m:e>${mathRun('2')}</m:e></m:mr><m:mr><m:e>${mathRun('3')}</m:e><m:e>${mathRun('4')}</m:e></m:mr></m:m></m:e></m:d>`, ''],
  ]
  for (let index = 0; index < rows.length; index++) {
    addSlideTextBox(slide, { x: inches(0.35), y: inches(0.25 + index * 0.95), w: inches(9), h: inches(0.9), text: `EQUATION${index}` })
  }
  const archive = unzipSync(await savePresentation(pres))
  const name = 'ppt/slides/slide1.xml'
  let xml = strFromU8(archive[name])
  const normalRun = text => `<a:r><a:rPr sz="2400"><a:solidFill><a:srgbClr val="000000"/></a:solidFill></a:rPr><a:t>${text}</a:t></a:r>`
  for (const [index, [before, body, after]] of rows.entries()) {
    const equation = body.replace('<m:r>', '<m:r><a:rPr sz="2400"><a:solidFill><a:srgbClr val="d60000"/></a:solidFill></a:rPr>')
    const math = `<a14:m xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main" xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math"><m:oMathPara><m:oMath>${equation}</m:oMath></m:oMathPara></a14:m>`
    const alternate = `<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006" xmlns:a14="http://schemas.microsoft.com/office/drawing/2010/main"><mc:Choice Requires="a14">${math}</mc:Choice><mc:Fallback>${normalRun('FALLBACK MUST NOT DUPLICATE')}</mc:Fallback></mc:AlternateContent>`
    xml = xml.replace(new RegExp(`<a:r>(?:(?!<a:r>)[\\s\\S])*?<a:t>EQUATION${index}</a:t></a:r>`), normalRun(before) + (index === 0 ? alternate : math) + normalRun(after))
  }
  archive[name] = strToU8(xml)
  return zipSync(archive)
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await writeFile(process.argv[2], await createFixture())
}
