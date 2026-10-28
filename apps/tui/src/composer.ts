import { TextareaRenderable, type TextareaOptions, type RenderContext } from '@opentui/core';

/** Multiline OpenTUI editor with the former input.value interface for callers. */
export class ComposerRenderable extends TextareaRenderable {
  readonly maxLength = 16000;
  constructor(ctx: RenderContext, options: TextareaOptions) {
    super(ctx, { ...options, wrapMode: 'word', minHeight: 1, maxHeight: 6,
      keyBindings: [
        { name: 'return', action: 'submit' },
        { name: 'return', shift: true, action: 'newline' },
        { name: 'return', meta: true, action: 'newline' },
      ],
    });
  }
  get value(): string { return this.plainText; }
  set value(value: string) { this.setText(value.slice(0, this.maxLength)); this.gotoBufferEnd(); }
  override insertText(text: string): void {
    super.insertText(text.slice(0, Math.max(0, this.maxLength - this.plainText.length)));
  }
}
