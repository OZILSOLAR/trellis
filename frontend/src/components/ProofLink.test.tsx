import { render, screen } from '@testing-library/react'
import { describe, expect, it } from 'vitest'

import ProofLink from './ProofLink'

describe('<ProofLink />', () => {
  it('links a safe https proof', () => {
    render(<ProofLink uri="https://example.com/proof" label="View" />)

    const link = screen.getByRole('link', { name: 'View' })
    expect(link).toHaveAttribute('href', 'https://example.com/proof')
    expect(link).toHaveAttribute('rel', 'noopener noreferrer')
  })

  it.each(['javascript:alert(1)', 'JaVaScRiPt:alert(1)', 'java\tscript:alert(1)', 'data:text/html,x', '//evil.example'])(
    'renders %j as inert text with no link',
    (uri) => {
      const { container } = render(<ProofLink uri={uri} label="View" />)

      expect(screen.queryByRole('link')).toBeNull()
      expect(container.querySelector('[href]')).toBeNull()
      expect(screen.getByTestId('untrusted-proof')).toHaveTextContent('Untrusted link:')
    },
  )
})
