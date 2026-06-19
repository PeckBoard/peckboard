import { useEffect, type CSSProperties, type MouseEvent, type ReactNode } from 'react'
import { createPortal } from 'react-dom'

interface ModalProps {
  /**
   * Called when the user dismisses the modal (Escape, backdrop click, or
   * an explicit close button). When omitted, the modal is non-dismissible
   * (used by the login modal, which the app forces open until the user
   * authenticates).
   */
  onClose?: () => void
  /** Convenience: applied as `max-width` on the inner `.modal` panel. */
  maxWidth?: number | string
  /** Extra class names appended to the inner `.modal` panel. */
  className?: string
  /** Extra class names appended to the `.modal-backdrop`. */
  backdropClassName?: string
  /** Extra inline style on the inner `.modal` panel. */
  style?: CSSProperties
  /** Close when the user clicks outside the modal. Defaults to true when `onClose` is set. */
  closeOnBackdropClick?: boolean
  /** Close when the user presses Escape. Defaults to true when `onClose` is set. */
  closeOnEscape?: boolean
  /** Passed through to the inner `.modal` panel. */
  'data-testid'?: string
  children: ReactNode
}

/**
 * Portal-rendered modal shell. All app modals should go through this so
 * they escape any scrollable / transformed ancestor (e.g. the kanban
 * board's horizontal scroller) and live as a direct child of `body`.
 *
 * The inner panel does NOT clamp its own height — the backdrop
 * (`.modal-backdrop`) is the scroll container, so long forms cause the
 * page (backdrop) to scroll while the panel itself flows naturally.
 */
export default function Modal({
  onClose,
  maxWidth,
  className,
  backdropClassName,
  style,
  closeOnBackdropClick,
  closeOnEscape,
  children,
  ...rest
}: ModalProps) {
  const dismissible = !!onClose
  const handleEscape = closeOnEscape ?? dismissible
  const handleBackdrop = closeOnBackdropClick ?? dismissible

  useEffect(() => {
    if (!handleEscape || !onClose) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [handleEscape, onClose])

  const onBackdropMouseDown = (e: MouseEvent) => {
    if (!handleBackdrop || !onClose) return
    if (e.target === e.currentTarget) onClose()
  }

  const panelStyle: CSSProperties = { ...(style ?? {}) }
  if (maxWidth !== undefined) {
    panelStyle.maxWidth = typeof maxWidth === 'number' ? `${maxWidth}px` : maxWidth
  }

  const backdropClasses = backdropClassName
    ? `modal-backdrop ${backdropClassName}`
    : 'modal-backdrop'
  const panelClasses = className ? `modal ${className}` : 'modal'

  return createPortal(
    <div className={backdropClasses} onMouseDown={onBackdropMouseDown}>
      <div
        className={panelClasses}
        onMouseDown={(e) => e.stopPropagation()}
        onClick={(e) => e.stopPropagation()}
        style={panelStyle}
        data-testid={rest['data-testid']}
      >
        {children}
      </div>
    </div>,
    document.body,
  )
}
