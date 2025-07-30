FROM ruby:2.7.3-alpine

WORKDIR /app

RUN apk update \
    && apk add openssh-client git bash keychain curl build-base \
    && rm -rf /var/cache/apk/*

RUN gem install --no-document \
    capistrano:3.16 \
    faraday:1.3 \
    ed25519:1.3 \
    bcrypt_pbkdf:1.1

RUN mkdir /root/.ssh
RUN chmod 744 /root/.ssh
RUN echo $'\n\
export PATH="/usr/local/bundle/bin:$PATH" \n\
if [ -e "/ssh/id_rsa.pub" ]; then \n\
    cp /ssh/id_rsa.pub ~/.ssh/id_rsa.pub \n\
    chmod 644 /root/.ssh/id_rsa.pub \n\
fi \n\
if [ -e "/ssh/id_rsa" ]; then \n\
    cp /ssh/id_rsa ~/.ssh/id_rsa \n\
    chmod 600 ~/.ssh/id_rsa \n\
    eval $(keychain --eval id_rsa) \n\
fi' >> /root/.profile