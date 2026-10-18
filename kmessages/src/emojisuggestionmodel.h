// SPDX-License-Identifier: SSPL-1.0
#pragma once

#include <QAbstractListModel>
#include <QPointer>

namespace km
{
class AppSettings;

/// Emoji for a typed word, from the bundled CLDR keyword table
/// (`qrc:/kmessages/data/emoji-keywords.json`, loaded once per process).
///
/// setWord() ranks the matches in three tiers, exact keyword, keyword
/// prefix, substring, with the short name ahead in each: the short name
/// equal to the word, a word of the short name equal to it ("red heart"),
/// another keyword equal to it, a word of the short name starting with it,
/// another keyword starting with it, a word inside a keyword starting with
/// it, any keyword containing it (that last rank only for the `:colon:` form
/// unless `plainWordSubstrings`). Within a
/// rank the shorter short name first, then the table's order (emoji
/// keyboard order; stable). The user's
/// recent emoji (AppSettings::recentEmoji) that match come first. At most
/// `limit` rows. The word may be in the `:snake:` form (colons stripped);
/// `_` matches a space.
///
/// The composer uses update(text, cursor): it finds the word before the
/// cursor (wordBefore()) and remembers where it is (`wordStart`,
/// `wordLength`) so the chosen emoji can replace it.
class EmojiSuggestionModel : public QAbstractListModel
{
    Q_OBJECT
    Q_PROPERTY(int count READ rowCount NOTIFY countChanged)
    Q_PROPERTY(int limit READ limit WRITE setLimit NOTIFY limitChanged)
    /// Characters a word needs without the colon form (the colon form needs 2).
    Q_PROPERTY(int minimumLength READ minimumLength WRITE setMinimumLength NOTIFY minimumLengthChanged)
    /// Plain words (not the `:colon:` form) also match inside keywords
    /// ("now" → ☃️ snowman). The composer turns this off: a plain word then
    /// matches only at the start of a keyword word.
    Q_PROPERTY(bool plainWordSubstrings READ plainWordSubstrings WRITE setPlainWordSubstrings NOTIFY optionsChanged)
    /// Words must start with a letter (the picker's search allows "100").
    Q_PROPERTY(bool requireLetterStart READ requireLetterStart WRITE setRequireLetterStart NOTIFY optionsChanged)
    /// The current word is in the `:colon:` form (explicit intent).
    Q_PROPERTY(bool colonForm READ colonForm NOTIFY wordChanged)
    /// For the recent emoji (optional).
    Q_PROPERTY(km::AppSettings *settings READ settings WRITE setSettings NOTIFY settingsChanged)
    /// The query matched (lower case, colons stripped), "" = none.
    Q_PROPERTY(QString query READ query NOTIFY wordChanged)
    /// After update(): the word's place in the text (colons included), -1 / 0 = none.
    Q_PROPERTY(int wordStart READ wordStart NOTIFY wordChanged)
    Q_PROPERTY(int wordLength READ wordLength NOTIFY wordChanged)
    /// After update(): the word as typed (e.g. ":snake").
    Q_PROPERTY(QString wordText READ wordText NOTIFY wordChanged)

public:
    enum Roles {
        EmojiRole = Qt::UserRole + 1,
        NameRole, ///< CLDR short name ("snake")
    };
    Q_ENUM(Roles)

    struct Span {
        int start = -1;
        int length = 0;
        QString query; ///< the word without colons
        bool colonForm = false;
    };

    explicit EmojiSuggestionModel(QObject *parent = nullptr);

    int rowCount(const QModelIndex &parent = QModelIndex()) const override;
    QVariant data(const QModelIndex &index, int role) const override;
    QHash<int, QByteArray> roleNames() const override;

    int limit() const { return m_limit; }
    void setLimit(int limit);
    int minimumLength() const { return m_minimumLength; }
    void setMinimumLength(int length);
    bool plainWordSubstrings() const { return m_plainWordSubstrings; }
    void setPlainWordSubstrings(bool on);
    bool requireLetterStart() const { return m_requireLetterStart; }
    void setRequireLetterStart(bool on);
    bool colonForm() const { return m_colonForm; }
    AppSettings *settings() const { return m_settings; }
    void setSettings(AppSettings *settings);
    QString query() const { return m_query; }
    int wordStart() const { return m_span.start; }
    int wordLength() const { return m_span.length; }
    QString wordText() const { return m_wordText; }

    /// Matches for `word` ("snake", ":snake", ":snake:"); too short or not a
    /// word: no rows.
    Q_INVOKABLE void setWord(const QString &word);
    /// setWord() for the word that ends at `cursor` in `text`, or clear().
    Q_INVOKABLE void update(const QString &text, int cursor);
    Q_INVOKABLE void clear();
    Q_INVOKABLE QString emojiAt(int row) const;

    /// The word ending at `cursor`: letters, digits and `_`, starting with a
    /// letter, at the start of the text or after a space / opening bracket or
    /// quote, the cursor not inside a word; or `:` + such a word (+ an
    /// optional closing `:`). Too short (plain: `minimumLength`, colon form:
    /// 2): start -1.
    static Span wordBefore(const QString &text, int cursor, int minimumLength = DefaultMinimumLength);
    /// `word` lower case, colons stripped, `_` → space; "" when it is not a
    /// word or too short (see wordBefore()).
    static QString normalize(const QString &word, int minimumLength = DefaultMinimumLength, bool requireLetterStart = true);
    /// Entries in the bundled table (tests).
    static int tableSize();

    static constexpr int DefaultLimit = 8;
    static constexpr int DefaultMinimumLength = 3;

Q_SIGNALS:
    void countChanged();
    void limitChanged();
    void minimumLengthChanged();
    void settingsChanged();
    void optionsChanged();
    void wordChanged();

private:
    void search();

    QList<int> m_rows; ///< indexes into the table
    int m_limit = DefaultLimit;
    int m_minimumLength = DefaultMinimumLength;
    bool m_plainWordSubstrings = true;
    bool m_requireLetterStart = true;
    bool m_colonForm = false;
    QPointer<AppSettings> m_settings;
    QStringList m_recent; ///< m_settings->recentEmoji(), kept up to date
    QString m_query;
    QString m_wordText;
    Span m_span;
};

} // namespace km
