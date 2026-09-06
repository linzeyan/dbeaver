import SwiftUI

/// Who this connection is on this server, and what that identity may do.
///
/// The question asked in the minute after a statement was refused: which user am
/// I connected as, which role is in force, may I create a table here. It is not
/// a user directory and there is nothing in it to press — every row is something
/// the server had already decided before it let this connection open, and
/// changing any of it is a GRANT the Query tab already runs.
///
/// Smaller than the two sheets beside it and with no filter, because the shape
/// of the answer is different: a server reports six hundred settings and this
/// reports six rows. A filter over six rows is a control that only ever hides
/// something.
///
/// The rows are whatever the driver sent, in the order it sent them, and this
/// side knows nothing about what they mean. That is what `InfoField` buys and
/// why it is the type here: PostgreSQL says "Role attributes" and another engine
/// will say something its own documentation uses, and a struct of named fields
/// would have made this sheet the place every engine's vocabulary had to meet.
struct LoginInfoSheet: View {
    @Bindable var model: AppModel

    private var fields: [InfoField] { model.loginInfo }

    var body: some View {
        VStack(spacing: 0) {
            header
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            body(of: fields)
            Rectangle().fill(Theme.Border.hairline.color).frame(height: 1)
            footer
        }
        // Sized for six rows and held there, so an ordinary four-row connection
        // opens with room below the last one. Three of the rows are conditional
        // — `Logged in as` after a SET ROLE, `Member of` for a role in a group,
        // `Database` at all — and Refresh is right there in the header, so a
        // sheet that fit its content would change size under the button that
        // asked it to. Slack is the cheaper of the two.
        .frame(width: 460, height: 300)
        .background(Theme.Surface.raised.color)
        .onExitCommand { model.closeLoginInfo() }
    }

    /// Names the connection rather than the sheet.
    ///
    /// "Connection Privileges" is already on the menu item that opened this, and
    /// a window with several tabs open makes *which* connection the part worth
    /// repeating: the answer below is true of one of them and of none of the
    /// others.
    private var header: some View {
        HStack(spacing: Theme.Space.sm) {
            Text(model.connectionLabel)
                .font(Theme.Typography.body)
                .foregroundStyle(Theme.Text.primary.color)
                .lineLimit(1)
                .truncationMode(.middle)

            Spacer(minLength: Theme.Space.sm)

            Button("Refresh") { model.loadLoginInfo() }
                .buttonStyle(.plain)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Accent.selection.color)
                .disabled(model.isReadingLoginInfo)
        }
        .padding(Theme.Space.md)
    }

    @ViewBuilder
    private func body(of fields: [InfoField]) -> some View {
        if fields.isEmpty {
            // Both empties in one sentence, deliberately. A file has no login to
            // report and a driver that was never taught to ask reports the same
            // nothing, and this side cannot tell them apart — a sentence that
            // picked one would be guessing in front of the person who asked.
            Text(
                model.isReadingLoginInfo
                    ? "Asking the server…"
                    : "This connection reports no login of its own. Nothing signs in to a "
                        + "file, and not every engine names the identity behind a connection."
            )
            .font(Theme.Typography.caption)
            .foregroundStyle(Theme.Text.tertiary.color)
            .padding(Theme.Space.md)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        } else {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(fields) { field in
                        row(field)
                    }
                }
                .padding(.vertical, Theme.Space.xs)
            }
        }
    }

    /// One field. The value is selectable and the label is not, for the reason
    /// the Structure tab's Info table gives: a role name is a thing to paste
    /// into a ticket, and "Connected as" is a word this window wrote.
    private func row(_ field: InfoField) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: Theme.Space.sm) {
            Text(field.label)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
                .frame(width: 130, alignment: .leading)
            // Wraps rather than truncating. A list of role names or of granted
            // rights is as long as it is, and the one that runs past the column
            // is the one somebody opened this to read.
            Text(field.value)
                .font(Theme.Typography.mono)
                .foregroundStyle(Theme.Text.primary.color)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, Theme.Space.md)
        .padding(.vertical, Theme.Space.xs)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(field.label), \(field.value)")
    }

    private var footer: some View {
        HStack(spacing: Theme.Space.sm) {
            if !model.loginReport.isEmpty {
                Text(model.loginReport)
                    .font(Theme.Typography.micro)
                    .foregroundStyle(Theme.Text.secondary.color)
                    .lineLimit(1)
            }
            Spacer(minLength: Theme.Space.sm)
            Button("Done") { model.closeLoginInfo() }
                .buttonStyle(.plain)
                .font(Theme.Typography.caption)
                .foregroundStyle(Theme.Text.secondary.color)
        }
        .padding(.horizontal, Theme.Space.md)
        .frame(height: 30)
    }
}
