// SPDX-License-Identifier: SSPL-1.0
#include "qmlsetup.h"
#include "appcontroller.h"

#include <KLocalizedQmlContext>

#include <QImageReader>
#include <QPainter>
#include <QPainterPath>
#include <QQmlEngine>

using namespace Qt::StringLiterals;

namespace km
{

void registerQmlTypes(AppController *controller)
{
    static bool registered = false;
    if (registered) {
        return;
    }
    registered = true;
    constexpr const char *uri = "org.imessage.kmessages";
    const QString reason = u"created by the application"_s;
    qmlRegisterUncreatableType<DaemonClient>(uri, 1, 0, "DaemonClient", reason);
    qmlRegisterUncreatableType<ChatListModel>(uri, 1, 0, "ChatListModel", reason);
    qmlRegisterUncreatableType<ChatFilterModel>(uri, 1, 0, "ChatFilterModel", reason);
    qmlRegisterUncreatableType<MessageListModel>(uri, 1, 0, "MessageListModel", reason);
    qmlRegisterUncreatableType<HandleLookupModel>(uri, 1, 0, "HandleLookupModel", reason);
    qmlRegisterUncreatableType<MessageSearchModel>(uri, 1, 0, "MessageSearchModel", reason);
    qmlRegisterUncreatableType<AppSettings>(uri, 1, 0, "AppSettings", reason);
    qmlRegisterUncreatableType<ContactResolver>(uri, 1, 0, "ContactResolver", reason);
    qmlRegisterUncreatableType<ContactSuggestionModel>(uri, 1, 0, "ContactSuggestionModel", reason);
    qmlRegisterSingletonInstance(uri, 1, 0, "App", controller);
}

void setupQmlEngine(QQmlEngine &engine, AppController *controller)
{
    KLocalization::setupLocalizedContext(&engine);
    registerQmlTypes(controller);
    // Contact photos for Avatar { source: model.avatarSource }
    engine.addImageProvider(u"contact"_s, new ContactImageProvider(controller->contacts()->shared()));
    engine.addImageProvider(u"attachment"_s, new AttachmentImageProvider);
}

AttachmentImageProvider::AttachmentImageProvider()
    : QQuickImageProvider(QQuickImageProvider::Image, QQmlImageProviderBase::ForceAsynchronousImageLoading)
{
}

QImage AttachmentImageProvider::requestImage(const QString &id, QSize *size, const QSize &requestedSize)
{
    // id = <percent-encoded path>[?rw=<corner radius as a fraction of the width>]
    QString path = id;
    qreal radiusFraction = 0;
    if (const auto q = id.lastIndexOf(u"?rw="_s); q >= 0) {
        radiusFraction = id.mid(q + 4).toDouble();
        path = id.left(q);
    }
    QImageReader reader(QUrl::fromPercentEncoding(path.toUtf8()));
    reader.setAutoTransform(true);
    QSize full = reader.size();
    if (full.isValid() && (reader.transformation() & (QImageIOHandler::TransformationRotate90))) {
        full.transpose();
    }
    // Decode at the needed size when the format supports it (JPEG does).
    if (full.isValid() && requestedSize.width() > 0 && requestedSize.width() < full.width()) {
        const QSize scaled = full.scaled(requestedSize.width(), full.height(), Qt::KeepAspectRatio);
        QSize raw = scaled;
        if (reader.transformation() & QImageIOHandler::TransformationRotate90) {
            raw.transpose();
        }
        reader.setScaledSize(raw);
    }
    QImage img = reader.read();
    if (!img.isNull() && requestedSize.width() > 0 && img.width() > requestedSize.width()) {
        img = img.scaledToWidth(requestedSize.width(), Qt::SmoothTransformation);
    }
    if (!img.isNull() && radiusFraction > 0) {
        QImage rounded(img.size(), QImage::Format_ARGB32_Premultiplied);
        rounded.fill(Qt::transparent);
        QPainter p(&rounded);
        p.setRenderHint(QPainter::Antialiasing);
        p.setRenderHint(QPainter::SmoothPixmapTransform);
        QPainterPath clip;
        const qreal r = radiusFraction * img.width();
        clip.addRoundedRect(QRectF(QPointF(0, 0), QSizeF(img.size())), r, r);
        p.setClipPath(clip);
        p.drawImage(0, 0, img);
        p.end();
        img = rounded;
    }
    if (size) {
        *size = img.size();
    }
    return img;
}

QUrl mainQmlUrl()
{
    return QUrl(u"qrc:/kmessages/qml/Main.qml"_s);
}

}
