import {
  useEffect,
  useId,
  useState,
  type CSSProperties,
  type MouseEvent,
  type ReactNode,
} from 'react'
import { createPortal } from 'react-dom'
import useDialogFocus from '../hooks/useDialogFocus'

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
  /** Accessible name when the modal has no heading of its own. */
  ariaLabel?: string
  /** Id of an existing element that names the modal. Overrides the
   *  heading auto-detection below. */
  labelledBy?: string
  /** Id of an element that describes the modal (e.g. its body copy),
   *  announced after the name. */
  describedBy?: string
  /** `alertdialog` marks an interruption that needs an answer before the
   *  user can go on (destructive confirmations); `dialog` otherwise. */
  role?: 'dialog' | 'alertdialog'
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
 *
 * The panel carries `role="dialog"` / `aria-modal`, is named from the
 * first heading it renders (or `ariaLabel` / `labelledBy`), and traps +
 * restores focus via `useDialogFocus`.
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
  ariaLabel,
  labelledBy,
  describedBy,
  role = 'dialog',
  children,
  ...rest
}: ModalProps) {
  const dismissible = !!onClose
  const handleEscape = closeOnEscape ?? dismissible
  const handleBackdrop = closeOnBackdropClick ?? dismissible

  const panelRef = useDialogFocus<HTMLDivElement>()
  const generatedLabelId = useId()
  const [detectedLabelId, setDetectedLabelId] = useState<string | null>(null)

  useEffect(() => {
    if (!handleEscape || !onClose) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [handleEscape, onClose])

  // Name the dialog from whatever heading it already renders, so every
  // existing consumer gets `aria-labelledby` without touching its markup.
  useEffect(() => {
    if (labelledBy || ariaLabel) return
    const panel = panelRef.current
    if (!panel) return
    const heading = panel.querySelector<HTMLElement>(
      '[data-dialog-title], .modal-title, h1, h2, h3, h4',
    )
    if (!heading) return
    if (!heading.id) heading.id = generatedLabelId
    setDetectedLabelId(heading.id)
  }, [labelledBy, ariaLabel, generatedLabelId, panelRef, children])

  const labelId = labelledBy ?? detectedLabelId

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
        ref={panelRef}
        className={panelClasses}
        role={role}
        aria-modal="true"
        aria-labelledby={labelId ?? undefined}
        aria-describedby={describedBy}
        aria-label={labelId ? undefined : (ariaLabel ?? 'Dialog')}
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
