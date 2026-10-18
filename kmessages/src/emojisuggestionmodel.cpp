// SPDX-License-Identifier: SSPL-1.0
#include "emojisuggestionmodel.h"
#include "appsettings.h"

#include <QFile>

#include <algorithm>
#include <QJsonArray>
#include <QJsonDocument>

using namespace Qt::StringLiterals;

namespace km
{

namespace
{
struct Entry {
    QString emoji;
    QStringList keywords; ///< lower case, the short name first
};

const QList<Entry> &table()
{
    static const QList<Entry> entries = [] {
        QList<Entry> out;
        QFile f(u":/kmessages/data/emoji-keywords.json"_s);
        if (!f.open(QIODevice::ReadOnly)) {
            qWarning("emoji-keywords.json is missing from the resources");
            return out;
        }
        const QJsonArray rows = QJsonDocument::fromJson(f.readAll()).array();
        out.reserve(rows.size());
        for (const auto &r : rows) {
            const QJsonArray pair = r.toArray();
            Entry e;
            e.emoji = pair.at(0).toString();
            const QJsonArray words = pair.at(1).toArray();
            for (const auto &w : words) {
                e.keywords << w.toString();
            }
            if (!e.emoji.isEmpty() && !e.keywords.isEmpty()) {
                out << e;
            }
        }
        return out;
    }();
    return entries;
}

bool isWordChar(QChar c)
{
    return c.isLetterOrNumber() || c == u'_';
}

/// May come right before a word (or its opening colon).
bool isBoundary(const QString &text, int pos)
{
    if (pos <= 0) {
        return true;
    }
    const QChar c = text.at(pos - 1);
    // An emoji (surrogates, U+FE0F, U+200D) ends a word too.
    return c.isSpace() || c.isSurrogate() || c == QChar(0xFE0F) || c == QChar(0x200D) || u"([{\"'«“‘"_s.contains(c);
}
}

EmojiSuggestionModel::EmojiSuggestionModel(QObject *parent)
    : QAbstractListModel(parent)
{
}

int EmojiSuggestionModel::tableSize()
{
    return int(table().size());
}

int EmojiSuggestionModel::rowCount(const QModelIndex &parent) const
{
    return parent.isValid() ? 0 : int(m_rows.size());
}

QHash<int, QByteArray> EmojiSuggestionModel::roleNames() const
{
    return {{EmojiRole, "emoji"}, {NameRole, "name"}};
}

QVariant EmojiSuggestionModel::data(const QModelIndex &index, int role) const
{
    if (!checkIndex(index, CheckIndexOption::IndexIsValid | CheckIndexOption::ParentIsInvalid)) {
        return {};
    }
    const Entry &e = table().at(m_rows.at(index.row()));
    switch (role) {
    case Qt::DisplayRole:
    case EmojiRole:
        return e.emoji;
    case NameRole:
        return e.keywords.first();
    }
    return {};
}

QString EmojiSuggestionModel::emojiAt(int row) const
{
    return row >= 0 && row < m_rows.size() ? table().at(m_rows.at(row)).emoji : QString();
}

void EmojiSuggestionModel::setLimit(int limit)
{
    if (limit != m_limit && limit > 0) {
        m_limit = limit;
        Q_EMIT limitChanged();
        search();
    }
}

void EmojiSuggestionModel::setMinimumLength(int length)
{
    if (length != m_minimumLength && length > 0) {
        m_minimumLength = length;
        Q_EMIT minimumLengthChanged();
    }
}

void EmojiSuggestionModel::setSettings(AppSettings *settings)
{
    if (settings == m_settings) {
        return;
    }
    if (m_settings) {
        disconnect(m_settings, nullptr, this, nullptr);
    }
    m_settings = settings;
    m_recent = settings ? settings->recentEmoji() : QStringList();
    if (settings) {
        connect(settings, &AppSettings::recentEmojiChanged, this, [this] {
            m_recent = m_settings ? m_settings->recentEmoji() : QStringList();
        });
    }
    Q_EMIT settingsChanged();
}

void EmojiSuggestionModel::setPlainWordSubstrings(bool on)
{
    if (on != m_plainWordSubstrings) {
        m_plainWordSubstrings = on;
        Q_EMIT optionsChanged();
        search();
    }
}

void EmojiSuggestionModel::setRequireLetterStart(bool on)
{
    if (on != m_requireLetterStart) {
        m_requireLetterStart = on;
        Q_EMIT optionsChanged();
    }
}

QString EmojiSuggestionModel::normalize(const QString &word, int minimumLength, bool requireLetterStart)
{
    QString w = word;
    bool colon = false;
    if (w.startsWith(u':')) {
        colon = true;
        w.remove(0, 1);
        if (w.endsWith(u':')) {
            w.chop(1);
        }
    }
    if (w.isEmpty() || (requireLetterStart && !w.at(0).isLetter()) || w.size() < (colon ? 2 : minimumLength)) {
        return {};
    }
    for (const QChar c : std::as_const(w)) {
        if (!isWordChar(c)) {
            return {};
        }
    }
    return w.toLower().replace(u'_', u' ');
}

EmojiSuggestionModel::Span EmojiSuggestionModel::wordBefore(const QString &text, int cursor, int minimumLength)
{
    Span none;
    if (cursor <= 0 || cursor > text.size()) {
        return none;
    }
    if (cursor < text.size() && isWordChar(text.at(cursor))) {
        return none; // inside a word
    }
    int end = cursor;
    const bool closingColon = text.at(end - 1) == u':';
    if (closingColon) {
        --end;
    }
    int start = end;
    while (start > 0 && isWordChar(text.at(start - 1))) {
        --start;
    }
    const int len = end - start;
    if (len == 0 || !text.at(start).isLetter()) {
        return none;
    }
    if (start > 0 && text.at(start - 1) == u':') {
        // :snake / :snake:
        if (len < 2 || !isBoundary(text, start - 1)) {
            return none;
        }
        return Span{start - 1, cursor - (start - 1), text.mid(start, len), true};
    }
    if (closingColon || len < minimumLength || !isBoundary(text, start)) {
        return none;
    }
    return Span{start, len, text.mid(start, len), false};
}

void EmojiSuggestionModel::setWord(const QString &word)
{
    const QString q = normalize(word, m_minimumLength, m_requireLetterStart);
    const bool colon = !q.isEmpty() && word.startsWith(u':');
    const bool changed = q != m_query || m_wordText != word || colon != m_colonForm;
    m_query = q;
    m_wordText = word;
    m_colonForm = colon;
    m_span = Span{};
    search();
    if (changed) {
        Q_EMIT wordChanged();
    }
}

void EmojiSuggestionModel::update(const QString &text, int cursor)
{
    const Span span = wordBefore(text, cursor, m_minimumLength);
    if (span.start < 0) {
        clear();
        return;
    }
    const QString typed = text.mid(span.start, span.length);
    const QString q = normalize(typed, m_minimumLength);
    const bool changed = q != m_query || typed != m_wordText || span.start != m_span.start || span.length != m_span.length || span.colonForm != m_colonForm;
    m_query = q;
    m_wordText = typed;
    m_colonForm = span.colonForm;
    m_span = span;
    search();
    if (changed) {
        Q_EMIT wordChanged();
    }
}

void EmojiSuggestionModel::clear()
{
    const bool changed = !m_query.isEmpty() || !m_wordText.isEmpty() || m_span.start >= 0;
    m_colonForm = false;
    m_query.clear();
    m_wordText.clear();
    m_span = Span{};
    search();
    if (changed) {
        Q_EMIT wordChanged();
    }
}

void EmojiSuggestionModel::search()
{
    QList<int> rows;
    if (!m_query.isEmpty()) {
        const auto &entries = table();
        // Ranks, three tiers (exact, prefix, substring), the short name
        // first in each: 0 the short name == q, 1 a word of the short name
        // == q ("red heart"), 2 another keyword == q, 3 a word of the short
        // name starts with q, 4 another keyword starts with q, 5 a word
        // inside a keyword starts with q, 6 a keyword contains q (plain
        // words only with plainWordSubstrings).
        constexpr int Ranks = 7;
        const bool substrings = m_colonForm || m_plainWordSubstrings;
        QList<int> ranked[Ranks];
        for (int i = 0; i < entries.size(); ++i) {
            const QStringList &kw = entries.at(i).keywords;
            const QString &name = kw.first();
            int rank = Ranks;
            if (name == m_query) {
                rank = 0;
            } else {
                const auto words = QStringView(name).split(u' ', Qt::SkipEmptyParts);
                for (const auto w : words) {
                    if (w == m_query) {
                        rank = 1;
                        break;
                    }
                    if (w.startsWith(m_query)) {
                        rank = std::min(rank, 3);
                    }
                }
            }
            for (int k = 1; k < kw.size() && rank > 2; ++k) {
                const QString &w = kw.at(k);
                if (w == m_query) {
                    rank = 2;
                } else if (w.startsWith(m_query)) {
                    rank = std::min(rank, 4);
                }
            }
            if (rank == Ranks) {
                for (const QString &w : kw) {
                    const qsizetype at = w.indexOf(m_query);
                    if (at < 0) {
                        continue;
                    }
                    if (at > 0 && w.at(at - 1).isLetterOrNumber()) {
                        rank = std::min(rank, 6); // inside a word
                        continue;
                    }
                    rank = 5;
                    break;
                }
                if (rank == 6 && !substrings) {
                    rank = Ranks;
                }
            }
            if (rank < Ranks) {
                ranked[rank] << i;
            }
        }
        // Within a rank the shorter short name first ("red heart" before
        // "heart with arrow", "cat face" before "grinning cat"), then the
        // table's order (stable).
        QList<int> matches;
        for (auto &r : ranked) {
            std::stable_sort(r.begin(), r.end(), [&entries](int a, int b) {
                return entries.at(a).keywords.first().size() < entries.at(b).keywords.first().size();
            });
            matches << r;
        }
        // Recent emoji that match go first, most recent first.
        if (m_settings) {
            for (const QString &emoji : std::as_const(m_recent)) {
                for (int j = 0; j < matches.size(); ++j) {
                    if (entries.at(matches.at(j)).emoji == emoji) {
                        rows << matches.takeAt(j);
                        break;
                    }
                }
            }
        }
        rows << matches;
        if (rows.size() > m_limit) {
            rows.resize(m_limit);
        }
    }
    if (rows == m_rows) {
        return;
    }
    beginResetModel();
    m_rows = rows;
    endResetModel();
    Q_EMIT countChanged();
}

} // namespace km
