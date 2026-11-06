import test from 'node:test'
import assert from 'node:assert/strict'
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { DOMParser } from '@xmldom/xmldom'
import { Resvg } from '@resvg/resvg-js'
import { unzipSync, zipSync, strFromU8, strToU8 } from 'fflate'
import { loadPresentation, getSlides, getSlideSize } from '@office-kit/pptx'
import { buildFontkitMeasurer } from '@office-kit/pptx-preview/node'
import { createMathFixture } from './pptx-fixture.mjs'
import { renderPptx, checkedSlideSvg } from './pptx.mjs'
import { ommlToLatex, preparePptxMath, equationMeasurer } from './pptx-math.mjs'

test('neutron slide keeps inline equations in both indexed text and rendered pixels', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'kb-inline-math-'))
  try {
    const input = await createMathFixture()
    const source = path.join(root, 'neutrons.pptx')
    await writeFile(source, input)
    const { pages } = await renderPptx(source, path.join(root, 'pages'))
    assert.equal(pages.length, 1)
    const text = pages[0].text
    assert.match(text, /中子寿命：\\\(\{10\}\^\{-4\}\\sim \{10\}\^\{-3\}\\\) s/)
    assert.match(text, /\\frac\{\{10\}\^\{16\}\}\{\{m\}\^\{3\}\}/)
    assert.match(text, /\\int\\nolimits_\{0\}\^\{\\infty \}/)
    assert.match(text, /\\begin\{matrix\}1 & 2 \\\\ 3 & 4\\end\{matrix\}/)
    assert.match(text, /随入射能量变化/)
    assert.doesNotMatch(text, /EQUATION|KBFormula|FALLBACK|[\ue000-\uf8ff]/)

    // Confirm paths at each inline position, rather than source code painted
    // under the slide or a text-only "successful" extraction.
    const math = preparePptxMath(input)
    const pres = await loadPresentation(math.input)
    const ordinary = buildFontkitMeasurer()
    const svg = checkedSlideSvg(pres, getSlides(pres)[0], 1, equationMeasurer(math, ordinary), math)
    assert.equal((svg.match(/data-knowledge-equation=/g) || []).length, 5)
    const document = new DOMParser().parseFromString(svg, 'application/xml')
    const equations = Array.from(document.getElementsByTagNameNS('http://www.w3.org/2000/svg', 'svg')).filter(node => node.hasAttribute('data-knowledge-equation'))
    assert.ok(Number(equations[0].getAttribute('width')) > 70)
    assert.ok(Number(equations[0].getAttribute('height')) > 20)
    assert.ok(Number(equations[0].getAttribute('y')) < Number(equations[1].getAttribute('y')))
    assert.ok(equations.every(node => node.getElementsByTagNameNS('http://www.w3.org/2000/svg', 'path').length > 0))

    const png = await readFile(path.join(root, 'pages', '1.png'))
    const bitmap = new Resvg(`<svg xmlns="http://www.w3.org/2000/svg" width="800" height="450"><image width="800" height="450" href="data:image/png;base64,${png.toString('base64')}"/></svg>`).render()
    const redByRow = Array(450).fill(0)
    const pixels = bitmap.pixels
    for (let pixel = 0; pixel < pixels.length; pixel += 4) {
      if (pixels[pixel] > 100 && pixels[pixel + 1] < 70 && pixels[pixel + 2] < 70) redByRow[Math.floor(pixel / 4 / 800)]++
    }
    const pixelsPerInch = 800 / (getSlideSize(pres).width / 914400)
    for (let row = 0; row < 5; row++) {
      const start = Math.floor((0.25 + row * 0.95) * pixelsPerInch)
      assert.ok(redByRow.slice(start, start + Math.ceil(0.9 * pixelsPerInch)).reduce((a, b) => a + b, 0) > 20, `equation ${row + 1} is missing from its slide row`)
    }
  } finally { await rm(root, { recursive: true, force: true }) }
})

test('unknown structures and XML entities fail explicitly instead of flattening equations', async () => {
  const archive = unzipSync(await createMathFixture())
  const name = 'ppt/slides/slide1.xml'
  const original = strFromU8(archive[name])
  archive[name] = strToU8(original.replace('<m:sSup>', '<m:unsupported>').replace('</m:sSup>', '</m:unsupported>'))
  assert.throws(() => preparePptxMath(zipSync(archive)), /不支持的结构/)
  archive[name] = strToU8(original.replace('<p:sld ', '<!DOCTYPE p:sld [<!ENTITY secret "hidden">]><p:sld '))
  assert.throws(() => preparePptxMath(zipSync(archive)), /声明/)
})

test('OMML bounds, literal TeX commands and normal-style units retain their meaning', () => {
  const parse = body => new DOMParser().parseFromString(`<m:oMath xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math">${body}</m:oMath>`, 'application/xml').documentElement
  assert.equal(ommlToLatex(parse('<m:r><m:t>\\input{https://example.invalid}</m:t></m:r>')), '\\backslash input\\{https://example.invalid\\}')
  assert.equal(ommlToLatex(parse('<m:r><m:rPr><m:nor/></m:rPr><m:t>MeV</m:t></m:r>')), '\\mathrm{MeV}')
  assert.equal(ommlToLatex(parse('<m:sPre><m:e><m:r><m:t>U</m:t></m:r></m:e><m:sub><m:r><m:t>92</m:t></m:r></m:sub><m:sup><m:r><m:t>235</m:t></m:r></m:sup></m:sPre>')), '{}_{92}^{235}{U}')
})
