import test from 'node:test'
import assert from 'node:assert/strict'
import { mkdtemp, writeFile, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { unzipSync, zipSync, strFromU8, strToU8 } from 'fflate'
import { Resvg } from '@resvg/resvg-js'
import { renderPptx } from './pptx.mjs'
import { createFixture } from './pptx-fixture.mjs'

test('code renderer preserves real slide order, CJK text, embedded image and native chart', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'kb-pptx-'))
  try {
    const source = path.join(root, 'course.pptx')
    await writeFile(source, await createFixture())
    const { pages } = await renderPptx(source, path.join(root, 'pages'))
    assert.equal(pages.length, 3)
    assert.equal(pages[0].text, '')
    assert.match(pages[1].text, /核反应截面 NEUTRON CROSS SECTION/)
    assert.match(pages[2].text, /CHART PAGE/)
    assert.deepEqual(pages.map(page => page.page), [1, 2, 3])
    for (const page of pages) {
      const png = await readFile(path.join(root, 'pages', page.imageFile))
      assert.equal(png.subarray(0, 8).toString('hex'), '89504e470d0a1a0a')
      assert.ok(png.length > 1000)
    }
    // Verify actual pixels, not a generic image placeholder.
    const first = await readFile(path.join(root, 'pages', '1.png'))
    const pixels = new Resvg(`<svg xmlns="http://www.w3.org/2000/svg" width="160" height="90"><image width="160" height="90" href="data:image/png;base64,${first.toString('base64')}"/></svg>`).render().pixels
    let red = 0
    for (let i = 0; i < pixels.length; i += 4) if (pixels[i] > 150 && pixels[i + 1] < 90 && pixels[i + 2] < 90) red++
    assert.ok(red > 2000, 'embedded source image was lost')
    await assert.rejects(renderPptx(source, path.join(root, 'limited'), 2), /页数/)
  } finally { await rm(root, { recursive: true, force: true }) }
})

test('unsupported Office math is explicitly rejected instead of silently dropped', async () => {
  const root = await mkdtemp(path.join(tmpdir(), 'kb-pptx-math-'))
  try {
    const zip = unzipSync(await createFixture())
    const name = 'ppt/slides/slide1.xml'
    const xml = strFromU8(zip[name]).replace('</p:spTree>', '<m:oMath xmlns:m="http://schemas.openxmlformats.org/officeDocument/2006/math"><m:r><m:t>x</m:t></m:r></m:oMath></p:spTree>')
    zip[name] = strToU8(xml)
    const source = path.join(root, 'math.pptx')
    await writeFile(source, zipSync(zip))
    await assert.rejects(renderPptx(source, path.join(root, 'pages')), /数学公式/)
  } finally { await rm(root, { recursive: true, force: true }) }
})
