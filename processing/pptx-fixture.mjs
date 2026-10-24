import { writeFile } from 'node:fs/promises'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { Resvg } from '@resvg/resvg-js'
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

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await writeFile(process.argv[2], await createFixture())
}
